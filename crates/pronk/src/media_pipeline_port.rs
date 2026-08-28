//! Application-owned boundaries for capture/PipeWire media construction.

use std::fmt;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::device_session_port::{DeviceMediaConfiguration, DeviceMediaTarget};
use crate::media_session::{MediaStartRequest, MediaStopReason, MediaSuspendReason};

/// Exact producer objects created for one active capture generation.
#[derive(Debug)]
pub struct PreparedCaptureMedia {
    pub media_generation: NonZeroU64,
    pub video_target: DeviceMediaTarget,
    pub audio_target: Option<DeviceMediaTarget>,
    pub configuration: DeviceMediaConfiguration,
}

/// Fresh consumer-class PipeWire connections for one backend generation.
#[derive(Debug)]
pub struct DeviceMediaRemoteSet {
    pub video: OwnedFd,
    pub audio: Option<OwnedFd>,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct MediaPipelineError(String);

impl MediaPipelineError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Sole-owner capture and producer-node boundary.
///
/// Its production adapter is the future per-display CastKMS actor. Neither the
/// media-session use case nor the Device backend can access a grant or DRM fd.
#[async_trait]
pub trait CapturePipelinePort: fmt::Debug + Send + 'static {
    async fn start(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<PreparedCaptureMedia, MediaPipelineError>;

    /// Admit frame production after the backend has configured its consumer
    /// and completed any transport negotiation performed before playback.
    async fn activate(
        &mut self,
        media_generation: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError>;

    async fn suspend(
        &mut self,
        media_generation: NonZeroU64,
        reason: MediaSuspendReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError>;

    async fn stop(
        &mut self,
        media_generation: NonZeroU64,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError>;

    async fn shutdown(
        &mut self,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError>;
}

/// Authority-limited connection minter. It creates consumer connections but
/// never sees capture buffers, a CastKMS grant, or backend protocol objects.
#[async_trait]
pub trait DeviceMediaRemotePort: fmt::Debug + Send + 'static {
    async fn mint(
        &mut self,
        media_generation: NonZeroU64,
        needs_audio: bool,
        cancellation: CancellationToken,
    ) -> Result<DeviceMediaRemoteSet, MediaPipelineError>;
}
