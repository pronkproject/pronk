//! One capture stream, with destination ownership independent of media transport.
//!
//! Allocation and display activation are external. The actor accepts fresh,
//! independently writable linear XRGB8888 allocations for one authorization
//! domain. Never recycle exported storage into a differently authorized session.

pub mod allocation;
mod names;
mod native;
mod pool;
mod session;
mod setup;
mod worker;

pub use pool::BufferHandle;
pub use session::Session;

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use drm_capture::RequestId;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Caller-allocated, independent storage. No source or display capability belongs here.
#[derive(Debug)]
pub struct Buffer {
    fd: OwnedFd,
    description: BufferDescription,
}

impl Buffer {
    /// Transfer exclusive write ownership of CPU-mappable linear XRGB8888.
    ///
    /// Distinct entries must not alias one another, and no other component may
    /// submit access except while holding the corresponding completed frame.
    pub fn new_mappable(fd: OwnedFd, pitch: NonZeroU32, size: NonZeroU64) -> Self {
        Self {
            fd,
            description: BufferDescription {
                format: u32::from_le_bytes(*b"XR24"),
                pitch,
                size,
                storage: BufferStorage::MappableLinear,
            },
        }
    }

    /// Transfer exclusive write ownership of a graphics-allocated DMA-BUF.
    pub fn new_drm(
        fd: OwnedFd,
        format: u32,
        modifier: u64,
        pitch: NonZeroU32,
        offset: u32,
        size: NonZeroU64,
    ) -> io::Result<Self> {
        if format == 0 || u64::from(offset) >= size.get() {
            return Err(invalid("invalid capture buffer description"));
        }
        Ok(Self {
            fd,
            description: BufferDescription {
                format,
                pitch,
                size,
                storage: BufferStorage::DrmModifier { modifier, offset },
            },
        })
    }

    pub fn description(&self) -> BufferDescription {
        self.description
    }
}

impl AsFd for Buffer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Transport-relevant layout of one packed single-plane destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferDescription {
    pub format: u32,
    pub pitch: NonZeroU32,
    pub size: NonZeroU64,
    pub storage: BufferStorage,
}

impl BufferDescription {
    pub fn modifier(self) -> u64 {
        match self.storage {
            BufferStorage::MappableLinear => 0,
            BufferStorage::DrmModifier { modifier, .. } => modifier,
        }
    }

    pub fn offset(self) -> u32 {
        match self.storage {
            BufferStorage::MappableLinear => 0,
            BufferStorage::DrmModifier { offset, .. } => offset,
        }
    }
}

/// How an authorized media recipient may describe the allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferStorage {
    MappableLinear,
    DrmModifier { modifier: u64, offset: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub capacity: NonZeroU32,
    /// Polling also observes completions after grant revocation. It is not a frame rate.
    pub poll_interval: Duration,
    pub shutdown_timeout: Duration,
}

impl Config {
    fn validate(self, buffers: usize) -> io::Result<()> {
        if !(1..=64).contains(&buffers)
            || self.capacity.get() as usize > buffers
            || self.poll_interval.is_zero()
            || self.shutdown_timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(self.poll_interval)
                .is_none()
            || std::time::Instant::now()
                .checked_add(self.shutdown_timeout)
                .is_none()
        {
            return Err(invalid("invalid capture pool or timing configuration"));
        }
        Ok(())
    }
}

/// Completed pixels, retaining their allocation and exclusive pool use.
///
/// Drop returns the slot. Finish CPU reads and enroll all submitted GPU reads
/// into the DMA-BUF reservation before dropping. Do not retain an exported fd
/// for future accesses after return. A descriptor duplicate is not a pool lease.
#[derive(Debug)]
pub struct Frame {
    buffer: Arc<Buffer>,
    slot: usize,
    request: RequestId,
    timestamp: Duration,
    layout: Layout,
    returned: Option<mpsc::UnboundedSender<usize>>,
}

impl Frame {
    pub fn request(&self) -> RequestId {
        self.request
    }

    pub fn timestamp(&self) -> Duration {
        self.timestamp
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn stride(&self) -> NonZeroU32 {
        self.buffer.description.pitch
    }

    /// Permanently withhold this destination from further captures in the actor.
    ///
    /// Use when a consumer's release is uncertain. Storage references are freed
    /// normally, but no pool credit is returned. Shutdown still drains admitted
    /// kernel work and does not wait for the withheld credit.
    pub fn retire(mut self) {
        self.returned = None;
    }
}

impl AsFd for Frame {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.buffer.as_fd()
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // Each non-cloneable frame returns exactly once; queued returns are
        // bounded by the pool, even though Drop cannot await channel capacity.
        if let Some(returned) = &self.returned {
            let _ = returned.send(self.slot);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no capture request was admitted: capacity or storage unavailable")]
    Backpressure,
    #[error("capture request {request:?} completed without valid pixels: {source}")]
    FrameFailed {
        request: RequestId,
        #[source]
        source: io::Error,
    },
    #[error("capture transport failed: {0}")]
    Transport(#[source] io::Error),
    #[error("capture actor stopped")]
    Stopped,
}

type Reply = oneshot::Sender<Result<Frame, CaptureError>>;

/// A bounded command interface. Only its worker owns the capture client.
///
/// Dropping starts shutdown, not immediate task abortion. Keep the runtime alive
/// for retirement, or call shutdown to join it and recover the descriptor owner.
pub struct Actor<F> {
    commands: mpsc::Sender<Reply>,
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<F>>>,
    layout: Layout,
    buffers: Vec<BufferHandle>,
}

impl<F: AsFd + Send + 'static> Actor<F> {
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Stable storage handles for registering a media transport before capture.
    /// Access is authorized only while retaining a matching completed `Frame`.
    pub fn buffers(&self) -> &[BufferHandle] {
        &self.buffers
    }

    /// Request one frame. Backpressure means no request was admitted.
    ///
    /// A dropped future does not permit early destination reuse; accepted work
    /// is drained and any undelivered frame is returned to the pool.
    pub async fn capture(&self) -> Result<Frame, CaptureError> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(send)
            .await
            .map_err(|_| CaptureError::Stopped)?;
        receive.await.map_err(|_| CaptureError::Stopped)?
    }

    /// Wait until the worker stops accepting commands, including idle failure.
    ///
    /// Closure does not establish ended kernel writes. Use `shutdown` to join
    /// retirement and observe its result. Canceling this wait has no effect.
    pub async fn closed(&self) {
        self.commands.closed().await;
    }

    /// Stop admission and drain the kernel stream before returning its owner.
    ///
    /// Timeout is an error, never proof of ended writes. Allocations are dropped,
    /// not repurposed. Frames held downstream keep their own storage references.
    pub async fn shutdown(mut self) -> io::Result<F> {
        self.stop.cancel();
        self.task
            .take()
            .expect("live actor owns worker")
            .await
            .map_err(|error| io::Error::other(error.to_string()))?
    }
}

impl<F> Drop for Actor<F> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod buffer_tests {
    use super::*;

    #[test]
    fn graphics_buffers_reject_out_of_allocation_offsets() {
        let fd = std::fs::File::open("/dev/null").unwrap().into();
        let error = Buffer::new_drm(
            fd,
            u32::from_le_bytes(*b"XR24"),
            0,
            NonZeroU32::new(4).unwrap(),
            4,
            NonZeroU64::new(4).unwrap(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn graphics_buffers_distinguish_explicit_linear_storage() {
        let buffer = Buffer::new_drm(
            std::fs::File::open("/dev/null").unwrap().into(),
            u32::from_le_bytes(*b"XR24"),
            0,
            NonZeroU32::new(4).unwrap(),
            0,
            NonZeroU64::new(4).unwrap(),
        )
        .unwrap();

        assert_eq!(
            buffer.description().storage,
            BufferStorage::DrmModifier {
                modifier: 0,
                offset: 0
            }
        );
    }
}
