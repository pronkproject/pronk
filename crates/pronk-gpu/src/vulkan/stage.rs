//! A source-reading copy accepts only an independently available destination.

use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::geometry::SourceRect;
use pronk_dmabuf::{export_dependencies, import_completion, Access, Completion, SyncFile};

use super::image::ImageState;
use super::submission::{require_success, Job};
use super::{Image, SourceImage};

mod geometry;
use geometry::Copy;

impl SourceImage {
    /// Read this use into independently available private staging storage.
    ///
    /// The caller supplies an exclusive private destination, with no downstream
    /// submissions racing its native dependency snapshot. Pending destination
    /// reuse returns `WouldBlock` before waiting for or reading the source.
    /// Source-producer waits are native dependencies, not downstream reuse waits.
    ///
    /// Run on a blocking graphics worker. The import is consumed and destroyed
    /// after native reading ends; successful return supplies the initialized
    /// staging image and actual completion covering only this source-to-stage
    /// operation. Source authority and protocol release remain caller duties.
    pub fn copy_into_waited(self, destination: Image) -> io::Result<(Image, SyncFile)> {
        let copy = Copy::whole(self.layout(), destination.layout())?;
        self.copy_waited(destination, copy)
    }

    /// Copy a visible integral crop over a cleared private staging background.
    ///
    /// Placement is unscaled and unrotated. Copied bytes, including pixel alpha,
    /// are preserved: this is not alpha blending or color conversion. An opaque
    /// source in the same encoded RGB domain is required to use this operation
    /// as an opaque plane composition. The background has opaque alpha.
    ///
    /// A mismatched crop or fully offscreen placement is rejected before source
    /// waits or native submission. A caller with no visible source should clear
    /// private storage without acquiring a source use. All source authority,
    /// destination availability and blocking-worker rules of `copy_into_waited`
    /// apply unchanged. Successful return initializes the complete destination.
    pub fn copy_region_into_waited(
        self,
        destination: Image,
        source: SourceRect,
        placement: [i32; 2],
        background: [u8; 3],
    ) -> io::Result<(Image, SyncFile)> {
        let copy = Copy::placed(
            self.layout(),
            destination.layout(),
            source,
            placement,
            background,
        )?;
        self.copy_waited(destination, copy)
    }

    fn copy_waited(self, destination: Image, copy: Copy) -> io::Result<(Image, SyncFile)> {
        if !Arc::ptr_eq(&self.device, &destination.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging copy needs one Vulkan device",
            ));
        }
        let output = destination.export()?;
        let src = nix::sys::stat::fstat(self.fd.as_raw_fd())?;
        let dst = nix::sys::stat::fstat(output.as_raw_fd())?;
        if src.st_dev == dst.st_dev && src.st_ino == dst.st_ino {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging must not alias the source allocation",
            ));
        }
        require_available(export_dependencies(output.as_fd(), Access::Write)?.completion()?)?;
        require_success(
            SyncFile::from_fd(self.producer.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        require_success(export_dependencies(self.fd.as_fd(), Access::Read)?.wait_blocking()?)?;
        let mut job = Job::new(Arc::clone(&self.device), (self, destination))?;
        let (source, destination) = job.resources();
        let command = job.command();
        let range = destination.color_range();
        let source_acquire = vk::ImageMemoryBarrier::default()
            .image(source.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(job.device.queue_family)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(range);
        let source_release = vk::ImageMemoryBarrier::default()
            .image(source.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(job.device.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(range);
        let acquire = [
            source_acquire,
            destination.acquire_barrier(vk::AccessFlags::TRANSFER_WRITE),
        ];
        let release = [
            source_release,
            destination.release_barrier(vk::AccessFlags::TRANSFER_WRITE),
        ];
        // SAFETY: Valid imported source region, distinct owned destination and
        // completed native dependencies. The job retains both allocations; the
        // caller excludes external reuse throughout reading and foreign release.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &acquire,
            );
            if let Some(rgb) = copy.background {
                let color = vk::ClearColorValue {
                    float32: [
                        f32::from(rgb[0]) / 255.0,
                        f32::from(rgb[1]) / 255.0,
                        f32::from(rgb[2]) / 255.0,
                        1.0,
                    ],
                };
                job.device.raw.cmd_clear_color_image(
                    command,
                    destination.raw,
                    vk::ImageLayout::GENERAL,
                    &color,
                    &[range],
                );
                // Order the background write before the overlapping copy write.
                let clear_done = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                job.device.raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[clear_done],
                    &[],
                    &[],
                );
            }
            job.device.raw.cmd_copy_image(
                command,
                source.raw,
                vk::ImageLayout::GENERAL,
                destination.raw,
                vk::ImageLayout::GENERAL,
                &[copy.region],
            );
            job.device.raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &release,
            );
        }
        job.submit()?;
        let completion = job.export_completion()?;
        if let Some(sync) = &completion {
            import_completion(job.resources().0.fd.as_fd(), Access::Read, sync)?;
            import_completion(output.as_fd(), Access::Write, sync)?;
        }
        let (source, mut destination) = job.finish()?;
        drop(source);
        let completion = match completion {
            Some(sync) => sync,
            None => export_dependencies(output.as_fd(), Access::Write)?,
        };
        require_success(
            SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        destination.state = ImageState::Released;
        Ok((destination, completion))
    }
}

fn require_available(completion: Option<Completion>) -> io::Result<()> {
    match completion {
        Some(completion) => require_success(completion),
        None => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "private staging destination still has native users",
        )),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod region_tests;
