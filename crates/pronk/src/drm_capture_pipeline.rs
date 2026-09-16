//! Generic DRM capture and PipeWire adapter for production media sessions.

mod setup;

use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use drm_capture::Access as CaptureAccess;
use pronk_capture::{Actor, Layout};
use pronk_capture_pipewire::{State as VideoState, Video};
use pronk_pipewire::{
    ClassifiedSocketRemoteProvider, VideoSourceConfig, MAX_VIDEO_BUFFERS, MIN_VIDEO_BUFFERS,
};
use tokio_util::sync::CancellationToken;

use self::setup::{CaptureOwner, Setup};
use crate::capture_health::{CaptureEvents, CaptureMonitor};
use crate::device_session_port::{DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaTarget};
use crate::media_pipeline_port::{
    CaptureEvent, CapturePipelinePort, MediaPipelineError, PreparedCaptureMedia,
};
use crate::media_session::{MediaStartRequest, MediaStopReason, MediaSuspendReason};

/// Immutable identity and resource policy for one display's capture pipeline.
#[derive(Debug, Clone)]
pub struct DrmCapturePipelineConfig {
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub session_id: String,
    pub device_instance: String,
    pub node_description: String,
    pub video_profile_id: String,
    pub video_bitrate: NonZeroU64,
    /// Media cadence, independent of the display mode's refresh rate.
    pub capture_rate_hz: NonZeroU32,
    pub pool_size: NonZeroU32,
    pub request_capacity: NonZeroU32,
    pub pool_byte_limit: NonZeroU64,
    pub heap_path: PathBuf,
    pub poll_interval: Duration,
    pub shutdown_timeout: Duration,
}

struct ActiveCapture {
    monitor: CaptureMonitor,
    generation: NonZeroU64,
    video: Video<CaptureOwner>,
}

/// Sole owner of capture access and its per-generation PipeWire producer.
pub struct DrmCapturePipeline {
    setup: Setup,
    producer_remotes: ClassifiedSocketRemoteProvider,
    config: DrmCapturePipelineConfig,
    active: Option<ActiveCapture>,
    events: tokio::sync::mpsc::UnboundedSender<CaptureEvent>,
}

impl std::fmt::Debug for DrmCapturePipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DrmCapturePipeline")
            .field("config", &self.config)
            .field(
                "active_generation",
                &self.active.as_ref().map(|active| active.generation),
            )
            .finish_non_exhaustive()
    }
}

impl DrmCapturePipeline {
    pub fn new(
        capture: CaptureAccess,
        producer_remotes: ClassifiedSocketRemoteProvider,
        config: DrmCapturePipelineConfig,
    ) -> (Self, CaptureEvents) {
        let (events, receive) = CaptureEvents::channel();
        (
            Self {
                setup: Setup::new(capture),
                producer_remotes,
                config,
                active: None,
                events,
            },
            receive,
        )
    }

    fn active(&self, generation: NonZeroU64) -> Result<&ActiveCapture, MediaPipelineError> {
        match &self.active {
            Some(active) if active.generation == generation => Ok(active),
            Some(active) => Err(MediaPipelineError::new(format!(
                "capture operation requested generation {generation}; active generation is {}",
                active.generation
            ))),
            None => Err(MediaPipelineError::new("no capture generation is active")),
        }
    }

    fn validate_config(&self) -> Result<(), MediaPipelineError> {
        if !(MIN_VIDEO_BUFFERS..=MAX_VIDEO_BUFFERS)
            .contains(&(self.config.pool_size.get() as usize))
            || self.config.request_capacity > self.config.pool_size
            || self.config.poll_interval.is_zero()
            || self.config.shutdown_timeout.is_zero()
        {
            return Err(MediaPipelineError::new(
                "invalid capture pool, queue, or timing configuration",
            ));
        }
        Ok(())
    }

    async fn prepare_video(
        &self,
        actor: Actor<CaptureOwner>,
        generation: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<Video<CaptureOwner>, MediaPipelineError> {
        let remote = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(MediaPipelineError::new("capture start was cancelled"));
            }
            result = self.producer_remotes.create_producer_remote() => result.map_err(|error| {
                MediaPipelineError::new(format!("connect PipeWire producer: {error}"))
            })?,
        };
        Video::prepare(
            actor,
            VideoSourceConfig {
                node_name: format!("pronk.video.{}.{generation}", self.config.session_id),
                node_description: self.config.node_description.clone(),
                session_id: self.config.session_id.clone(),
                device_instance: self.config.device_instance.clone(),
                connector_id: self.config.connector_id,
                output_index: self.config.output_index,
                media_generation: generation,
                refresh_hz: self.config.capture_rate_hz,
            },
            remote.into_remote(),
        )
        .await
        .map_err(|error| MediaPipelineError::new(format!("prepare capture video: {error}")))
    }

    fn media_target(
        &self,
        video: &Video<CaptureOwner>,
        layout: Layout,
        generation: NonZeroU64,
    ) -> DeviceMediaTarget {
        DeviceMediaTarget {
            kind: DeviceMediaKind::Video,
            node_name: video.identity().node_name.clone(),
            object_serial: video.identity().object_serial,
            session_id: self.config.session_id.clone(),
            device_instance: self.config.device_instance.clone(),
            connector_id: self.config.connector_id,
            output_index: self.config.output_index,
            media_generation: generation,
            render_device: None,
            caps: format!(
                "video/x-raw,format=BGRx,width={},height={},framerate={}/1",
                layout.width, layout.height, self.config.capture_rate_hz
            ),
        }
    }

    async fn stop_active(&mut self, generation: NonZeroU64) -> Result<(), MediaPipelineError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        if active.generation != generation {
            let actual = active.generation;
            self.active = Some(active);
            return Err(MediaPipelineError::new(format!(
                "capture stop requested generation {generation}; active generation is {actual}"
            )));
        }
        let ActiveCapture { monitor, video, .. } = active;
        monitor.cancel();
        let (video, monitor) = tokio::join!(video.shutdown(), monitor.shutdown());
        let video = video
            .map(drop)
            .map_err(|error| MediaPipelineError::new(format!("stop capture video: {error}")));
        match (video, monitor) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(video), Err(monitor)) => Err(MediaPipelineError::new(format!(
                "{video}; capture monitor cleanup also failed: {monitor}"
            ))),
        }
    }
}

#[async_trait]
impl CapturePipelinePort for DrmCapturePipeline {
    async fn start(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<PreparedCaptureMedia, MediaPipelineError> {
        if self.active.is_some() {
            return Err(MediaPipelineError::new(
                "a previous capture generation still requires cleanup",
            ));
        }
        self.validate_config()?;
        let generation = NonZeroU64::new(request.media_generation)
            .ok_or_else(|| MediaPipelineError::new("media generation must be nonzero"))?;
        let (actor, layout) = self
            .setup
            .create_actor(self.config.clone(), request, cancellation.clone())
            .await?;
        let video = self
            .prepare_video(actor, generation, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            let capture = video.shutdown().await.map_err(|error| {
                MediaPipelineError::new(format!("cancel capture video: {error}"))
            })?;
            drop(capture);
            return Err(MediaPipelineError::new("capture start was cancelled"));
        }
        if video.identity().media_generation != generation {
            let capture = video.shutdown().await.map_err(|error| {
                MediaPipelineError::new(format!("stop stale capture video: {error}"))
            })?;
            drop(capture);
            return Err(MediaPipelineError::new(
                "PipeWire source returned a stale media generation",
            ));
        }
        let target = self.media_target(&video, layout, generation);
        let monitor = monitor_capture(generation, video.subscribe(), self.events.clone());
        self.active = Some(ActiveCapture {
            monitor,
            generation,
            video,
        });
        Ok(PreparedCaptureMedia {
            media_generation: generation,
            video_target: target,
            audio_target: None,
            configuration: DeviceMediaConfiguration {
                video_profile_id: self.config.video_profile_id.clone(),
                audio_profile_id: None,
                mode: request.route.mode,
                video_bitrate: self.config.video_bitrate,
            },
        })
    }

    async fn activate(
        &mut self,
        media_generation: NonZeroU64,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.active(media_generation)?
            .video
            .activate()
            .await
            .map_err(|error| MediaPipelineError::new(format!("activate capture video: {error}")))
    }

    async fn suspend(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaSuspendReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.active(media_generation)?
            .video
            .suspend()
            .await
            .map_err(|error| MediaPipelineError::new(format!("suspend capture video: {error}")))
    }

    async fn stop(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaStopReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.stop_active(media_generation).await
    }

    async fn shutdown(
        &mut self,
        _reason: MediaStopReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        if let Some(generation) = self.active.as_ref().map(|active| active.generation) {
            self.stop_active(generation).await?;
        }
        Ok(())
    }
}

fn require_route_layout(
    layout: Layout,
    request: MediaStartRequest,
) -> Result<(), MediaPipelineError> {
    if layout.width.get() != request.route.mode.width
        || layout.height.get() != request.route.mode.height
    {
        return Err(MediaPipelineError::new(format!(
            "capture output is {}x{}; active route is {}x{}",
            layout.width, layout.height, request.route.mode.width, request.route.mode.height
        )));
    }
    Ok(())
}

fn monitor_capture(
    media_generation: NonZeroU64,
    state: tokio::sync::watch::Receiver<VideoState>,
    events: tokio::sync::mpsc::UnboundedSender<CaptureEvent>,
) -> CaptureMonitor {
    CaptureMonitor::watch(
        media_generation,
        state,
        events,
        |state| match state {
            VideoState::Active => None,
            VideoState::Failed(error) => Some(error.clone()),
            VideoState::Stopped => Some("capture video stopped unexpectedly".into()),
        },
        "capture video health channel closed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_pipeline_port::CaptureEventPort;
    use tokio::sync::watch;

    #[tokio::test]
    async fn capture_failure_reports_the_exact_media_generation() {
        let (state, receive) = watch::channel(VideoState::Active);
        let (send, mut events) = CaptureEvents::channel();
        let generation = NonZeroU64::new(21).unwrap();
        let monitor = monitor_capture(generation, receive, send);
        state.send_replace(VideoState::Failed("destination access failed".into()));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), events.next_event())
                .await
                .unwrap(),
            Some(CaptureEvent::Failed {
                media_generation: generation,
                error: "destination access failed".into(),
            })
        );
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stopped_capture_is_a_failure_without_orderly_shutdown() {
        let (_state, receive) = watch::channel(VideoState::Stopped);
        let (send, mut events) = CaptureEvents::channel();
        let monitor = monitor_capture(NonZeroU64::new(23).unwrap(), receive, send);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.next_event())
                .await
                .unwrap(),
            Some(CaptureEvent::Failed { error, .. }) if error.contains("stopped unexpectedly")
        ));
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn orderly_capture_shutdown_does_not_report_failure() {
        let (state, receive) = watch::channel(VideoState::Active);
        let (send, mut events) = CaptureEvents::channel();
        let monitor = monitor_capture(NonZeroU64::new(25).unwrap(), receive, send);
        monitor.cancel();
        state.send_replace(VideoState::Stopped);
        monitor.shutdown().await.unwrap();
        assert_eq!(events.next_event().await, None);
    }
}
