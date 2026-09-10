//! Native command ownership, independent of the operation being recorded.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{Completion, SyncFile};

use super::device::{native, DeviceInner};

pub(super) fn require_success(completion: Completion) -> io::Result<()> {
    match completion {
        Completion::Success => Ok(()),
        Completion::Failed(error) => Err(io::Error::other(format!(
            "native GPU dependency failed: {error}"
        ))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Recording,
    Submitting,
    Submitted,
    Retired,
}

impl State {
    fn after_submit(result: Result<(), vk::Result>) -> Self {
        // Device loss has successful-submission lifetime semantics: native
        // retirement must be attempted before destroying submitted resources.
        if matches!(result, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
            Self::Submitted
        } else {
            Self::Submitting
        }
    }
}

/// Resources move into the job before recording and leave only after completion.
pub(super) struct Job<T> {
    pub(super) device: Arc<DeviceInner>,
    resources: Option<T>,
    pool: vk::CommandPool,
    command: vk::CommandBuffer,
    fence: vk::Fence,
    signal: vk::Semaphore,
    state: State,
    exported: bool,
}

impl<T> Job<T> {
    pub(super) fn new(device: Arc<DeviceInner>, resources: T) -> io::Result<Self> {
        let mut job = Self {
            device,
            resources: Some(resources),
            pool: vk::CommandPool::null(),
            command: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            signal: vk::Semaphore::null(),
            state: State::Recording,
            exported: false,
        };
        let pool = vk::CommandPoolCreateInfo::default().queue_family_index(job.device.queue_family);
        // SAFETY: Live device and queried queue family, valid creation structures.
        unsafe {
            job.pool = job
                .device
                .raw
                .create_command_pool(&pool, None)
                .map_err(native)?;
            job.fence = job
                .device
                .raw
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(native)?;
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            job.signal = job
                .device
                .raw
                .create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut export),
                    None,
                )
                .map_err(native)?;
            let info = vk::CommandBufferAllocateInfo::default()
                .command_pool(job.pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            job.command = job
                .device
                .raw
                .allocate_command_buffers(&info)
                .map_err(native)?[0];
            job.device
                .raw
                .begin_command_buffer(
                    job.command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(native)?;
        }
        Ok(job)
    }

    pub(super) fn resources(&self) -> &T {
        self.resources.as_ref().expect("job owns resources")
    }

    pub(super) fn command(&self) -> vk::CommandBuffer {
        self.command
    }

    pub(super) fn submit(&mut self) -> io::Result<()> {
        if self.state != State::Recording {
            return Err(io::Error::other("job is not recording"));
        }
        self.state = State::Submitting;
        // SAFETY: Only the command buffer belonging to this job is ended/submitted.
        unsafe { self.device.raw.end_command_buffer(self.command) }.map_err(native)?;
        let commands = [self.command];
        let signals = [self.signal];
        let submit = [vk::SubmitInfo::default()
            .command_buffers(&commands)
            .signal_semaphores(&signals)];
        let _guard = self
            .device
            .submission
            .lock()
            .map_err(|_| io::Error::other("Vulkan submission lock poisoned"))?;
        // SAFETY: The queue was created at setup, and the mutex excludes concurrent
        // host submission. Command resources remain owned until native retirement.
        let result = unsafe {
            let queue = self
                .device
                .raw
                .get_device_queue(self.device.queue_family, 0);
            self.device.raw.queue_submit(queue, &submit, self.fence)
        };
        self.state = State::after_submit(result);
        result.map_err(native)
    }

    pub(super) fn export_completion(&mut self) -> io::Result<Option<SyncFile>> {
        if self.state != State::Submitted || self.exported {
            return Err(io::Error::other("job completion is not exportable"));
        }
        let external =
            ash::khr::external_semaphore_fd::Device::new(self.device.instance(), &self.device.raw);
        let export = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.signal)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        // SAFETY: Accepted work will signal this binary exportable semaphore.
        let fd = unsafe { external.get_semaphore_fd(&export) }.map_err(native)?;
        self.exported = true;
        match fd {
            -1 => Ok(None), // Already-completed Vulkan SYNC_FD sentinel.
            fd if fd >= 0 => {
                // SAFETY: Successful export transfers a new owned descriptor.
                SyncFile::from_fd(unsafe { OwnedFd::from_raw_fd(fd) }).map(Some)
            }
            _ => Err(io::Error::other("invalid exported Vulkan sync file")),
        }
    }

    pub(super) fn finish(mut self) -> io::Result<T> {
        if self.state != State::Submitted {
            return Err(io::Error::other("job was not submitted"));
        }
        self.wait()?;
        Ok(self.resources.take().expect("completed job owns resources"))
    }

    fn wait(&mut self) -> io::Result<()> {
        // SAFETY: The accepted submission owns this fence and its command resources.
        let result = unsafe {
            self.device
                .raw
                .wait_for_fences(&[self.fence], true, u64::MAX)
        };
        if matches!(result, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
            self.state = State::Retired;
        }
        result.map_err(native)
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        if self.state == State::Submitted {
            let _ = self.wait();
            if self.state == State::Submitted {
                // An unexplained wait error does not authorize destruction.
                // Retain resources and native command objects until process exit.
                std::mem::forget(self.resources.take());
                std::mem::forget(Arc::clone(&self.device));
                return;
            }
        }
        // SAFETY: No accepted work remains pending (device loss permits teardown).
        // Null handles cover partial construction; pool destruction frees commands.
        unsafe {
            self.device.raw.destroy_command_pool(self.pool, None);
            self.device.raw.destroy_semaphore(self.signal, None);
            self.device.raw.destroy_fence(self.fence, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vulkan::Device;
    use pronk_dmabuf::Completion;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Retained(Arc<AtomicBool>);
    impl Drop for Retained {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn device() -> Device {
        Device::open(std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node"))
            .unwrap()
    }

    #[test]
    fn device_loss_during_submission_requires_native_retirement() {
        assert!(State::after_submit(Ok(())) == State::Submitted);
        assert!(State::after_submit(Err(vk::Result::ERROR_DEVICE_LOST)) == State::Submitted);
        for error in [
            vk::Result::ERROR_OUT_OF_HOST_MEMORY,
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        ] {
            assert!(State::after_submit(Err(error)) == State::Submitting);
        }
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU selection"]
    fn native_job_returns_resources_only_after_submission() {
        let device = device();
        let released = Arc::new(AtomicBool::new(false));
        let mut job = Job::new(Arc::clone(&device.inner), Retained(Arc::clone(&released))).unwrap();
        assert!(!released.load(Ordering::SeqCst));
        assert!(Arc::ptr_eq(&job.resources().0, &released));
        assert_ne!(job.command(), vk::CommandBuffer::null());
        assert!(job.export_completion().is_err());
        job.submit().unwrap();
        assert!(job.submit().is_err());
        let completion = job.export_completion().unwrap();
        assert!(job.export_completion().is_err());
        let retained = job.finish().unwrap();
        if let Some(completion) = completion {
            assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
        }
        assert!(!released.load(Ordering::SeqCst));
        drop(retained);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU selection"]
    fn dropping_a_submitted_job_retires_native_resources() {
        let device = device();
        let released = Arc::new(AtomicBool::new(false));
        let mut job = Job::new(Arc::clone(&device.inner), Retained(Arc::clone(&released))).unwrap();
        job.submit().unwrap();
        let completion = job.export_completion().unwrap();
        drop(job);
        assert!(released.load(Ordering::SeqCst));
        if let Some(completion) = completion {
            assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
        }
        let unsubmitted = Job::new(Arc::clone(&device.inner), ()).unwrap();
        assert!(unsubmitted.finish().is_err());
    }
}
