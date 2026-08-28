//! PipeWire consumer-remote adapter for the media-session application port.

use std::num::NonZeroU64;

use async_trait::async_trait;
use pronk_pipewire::ClassifiedSocketRemoteProvider;
use tokio_util::sync::CancellationToken;

use crate::media_pipeline_port::{DeviceMediaRemotePort, DeviceMediaRemoteSet, MediaPipelineError};

#[derive(Debug)]
pub struct ClassifiedDeviceMediaRemotePort {
    provider: ClassifiedSocketRemoteProvider,
    session_id: String,
    backend_id: String,
}

impl ClassifiedDeviceMediaRemotePort {
    pub fn new(
        provider: ClassifiedSocketRemoteProvider,
        session_id: impl Into<String>,
        backend_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
            backend_id: backend_id.into(),
        }
    }
}

#[async_trait]
impl DeviceMediaRemotePort for ClassifiedDeviceMediaRemotePort {
    async fn mint(
        &mut self,
        media_generation: NonZeroU64,
        needs_audio: bool,
        cancellation: CancellationToken,
    ) -> Result<DeviceMediaRemoteSet, MediaPipelineError> {
        let remotes = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(MediaPipelineError::new("PipeWire remote minting was cancelled"));
            }
            result = self.provider.create_backend_remotes(
                &self.session_id,
                &self.backend_id,
                media_generation,
                needs_audio,
            ) => result.map_err(|error| {
                MediaPipelineError::new(format!("mint backend PipeWire remotes: {error}"))
            })?,
        };
        let (video, audio) = remotes.into_parts();
        Ok(DeviceMediaRemoteSet {
            video: video.into_owned_fd(),
            audio: audio.map(|remote| remote.into_owned_fd()),
        })
    }
}
