//! One capture stream, with destination ownership independent of media transport.
//!
//! Allocation and display activation are external. The actor accepts fresh,
//! independently writable linear XRGB8888 allocations for one authorization
//! domain. Never recycle exported storage into a differently authorized session.

pub mod allocation;
mod native;
mod worker;

use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use drm_capture::{Client, RequestId};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Caller-allocated, independent storage. No source or display capability belongs here.
#[derive(Debug)]
pub struct Buffer {
    fd: OwnedFd,
    stride: NonZeroU32,
}

impl Buffer {
    /// Transfer exclusive write ownership. The kernel validates size and layout.
    ///
    /// Distinct entries must not alias one another, and no other component may
    /// submit access except while holding the corresponding completed frame.
    pub fn new(fd: OwnedFd, stride: NonZeroU32) -> Self {
        Self { fd, stride }
    }
}

impl AsFd for Buffer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
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
    returned: mpsc::UnboundedSender<usize>,
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
        self.buffer.stride
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
        let _ = self.returned.send(self.slot);
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
}

impl<F: AsFd + Send + 'static> Actor<F> {
    /// Open a stream on a fresh client for the currently active output.
    ///
    /// Configuration changes require shutdown and a new session with fresh
    /// storage. No grant, primary DRM file or broker identity reaches consumers.
    pub fn spawn(client: Client<F>, buffers: Vec<Buffer>, config: Config) -> io::Result<Self> {
        config.validate(buffers.len())?;
        // Require a runtime context before opening kernel state. Its timers
        // must also be enabled for completion polling and shutdown deadlines.
        tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("capture actor requires a Tokio runtime"))?;
        let (backend, layout) = native::Native::open(client, &buffers, config)?;
        Ok(worker::spawn(backend, buffers, layout, config))
    }

    pub fn layout(&self) -> Layout {
        self.layout
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
