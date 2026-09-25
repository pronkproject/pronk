use std::fmt::Debug;
use std::num::NonZeroU64;

use async_trait::async_trait;
use pronk_media::{
    EncodedMediaReceivers, MediaGraphActor, MediaGraphConfiguration, MediaGraphError,
    MediaGraphStatistics,
};

const ENCODED_OUTPUT_CAPACITY: usize = 8;
const ENCODED_AUDIO_OUTPUT_CAPACITY: usize = 32;

#[async_trait]
pub(super) trait MediaGraphPort: Debug + Send + 'static {
    async fn configure(
        &mut self,
        configuration: MediaGraphConfiguration,
    ) -> Result<(), MediaGraphError>;
    async fn start(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn suspend(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn resume(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn request_key_frame(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn set_video_bitrate(
        &mut self,
        generation: NonZeroU64,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError>;
    async fn stop(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError>;
    async fn statistics(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError>;
    async fn shutdown(&mut self) -> Result<(), MediaGraphError>;
}

#[derive(Debug)]
pub(super) struct GStreamerMediaGraph {
    actor: Option<MediaGraphActor>,
}

impl GStreamerMediaGraph {
    pub(super) fn spawn() -> Result<(Self, EncodedMediaReceivers), MediaGraphError> {
        let (actor, outputs) = MediaGraphActor::spawn_with_media_output(
            ENCODED_OUTPUT_CAPACITY,
            ENCODED_AUDIO_OUTPUT_CAPACITY,
        )?;
        Ok((Self { actor: Some(actor) }, outputs))
    }

    fn actor(&self) -> Result<&MediaGraphActor, MediaGraphError> {
        self.actor
            .as_ref()
            .ok_or_else(|| MediaGraphError::new("Chromiacast media graph is shut down"))
    }
}

#[async_trait]
impl MediaGraphPort for GStreamerMediaGraph {
    async fn configure(
        &mut self,
        configuration: MediaGraphConfiguration,
    ) -> Result<(), MediaGraphError> {
        self.actor()?.configure(configuration).await
    }

    async fn start(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.start(generation).await
    }

    async fn suspend(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.suspend(generation).await
    }

    async fn resume(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.resume(generation).await
    }

    async fn request_key_frame(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.request_key_frame(generation).await
    }

    async fn set_video_bitrate(
        &mut self,
        generation: NonZeroU64,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        self.actor()?.set_video_bitrate(generation, bitrate).await
    }

    async fn stop(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.actor()?.stop(generation).await
    }

    async fn statistics(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.actor()?.statistics(generation).await
    }

    async fn shutdown(&mut self) -> Result<(), MediaGraphError> {
        let Some(actor) = self.actor.take() else {
            return Ok(());
        };
        actor.shutdown().await
    }
}
