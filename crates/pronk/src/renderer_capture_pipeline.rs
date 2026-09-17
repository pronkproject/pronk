//! Userspace-rendered capture behind the application media port.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use async_trait::async_trait;
use castkms_renderer::Renderer;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{
    ClassifiedSocketRemoteProvider, VideoBufferLayout, VideoBufferStorage, VideoFrameRate,
    VideoPixelFormat, VideoSourceConfig,
};
use pronk_renderer_pipewire::{
    ActiveRendererStream, RendererStream, RendererStreamConfig, RendererStreamError,
    RendererStreamState,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::capability_lease::CapabilityLease;
use crate::capture_health::{CaptureEvents, CaptureMonitor};
use crate::device_session_port::{
    DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaTarget, RenderDeviceIdentity,
};
use crate::media_pipeline_port::{
    CaptureEvent, CapturePipelinePort, MediaPipelineError, PreparedCaptureMedia,
};
use crate::media_session::{MediaStartRequest, MediaStopReason, MediaSuspendReason};
use crate::renderer_session::{OpenRenderer, RendererAccess, RendererSession};

/// Immutable GPU and media policy for one display's renderer pipeline.
#[derive(Debug, Clone)]
pub struct RendererCapturePipelineConfig {
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub session_id: String,
    pub device_instance: String,
    pub node_description: String,
    pub video_profile_id: String,
    pub video_bitrate: NonZeroU64,
    pub video_frame_rate: VideoFrameRate,
    pub output_modifier: u64,
    pub private_capacity: NonZeroUsize,
    pub output_capacity: NonZeroUsize,
}

enum Stream {
    Prepared(RendererStream<OwnedFd>),
    Active {
        // Stop health observation before dropping the stream requests shutdown.
        monitor: CaptureMonitor,
        stream: ActiveRendererStream<OwnedFd>,
    },
}

#[derive(Clone, Copy)]
enum EndpointRecovery {
    Retained,
    Closed,
}

struct Generation {
    id: NonZeroU64,
    stream: Stream,
}

/// Sole owner of renderer authority and its per-generation GPU producer.
pub struct RendererCapturePipeline {
    renderer: Option<Renderer<OwnedFd>>,
    render_node: PathBuf,
    renderer_session: RendererSession,
    producer_remotes: ClassifiedSocketRemoteProvider,
    config: RendererCapturePipelineConfig,
    generation: Option<Generation>,
    // Drop requests stream shutdown before returning authority to its issuer.
    renderer_lease: Option<CapabilityLease>,
    events: mpsc::UnboundedSender<CaptureEvent>,
}

impl RendererCapturePipeline {
    pub fn new(
        access: RendererAccess,
        producer_remotes: ClassifiedSocketRemoteProvider,
        config: RendererCapturePipelineConfig,
    ) -> std::io::Result<(Self, CaptureEvents)> {
        let OpenRenderer {
            renderer,
            lease,
            render_node,
            session,
        } = access.open()?;
        let (events, receive) = CaptureEvents::channel();
        Ok((
            Self {
                renderer: Some(renderer),
                renderer_lease: Some(lease),
                render_node,
                renderer_session: session,
                producer_remotes,
                config,
                generation: None,
                events,
            },
            receive,
        ))
    }

    async fn stop_generation(
        &mut self,
        id: NonZeroU64,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        let Some(generation) = self.generation.take() else {
            return Ok(());
        };
        if generation.id != id {
            let actual = generation.id;
            self.generation = Some(generation);
            return Err(MediaPipelineError::new(format!(
                "renderer stop requested generation {id}; active generation is {actual}"
            )));
        }
        match generation.stream {
            Stream::Prepared(stream) => {
                let stopped = stream
                    .shutdown()
                    .await
                    .map_err(|error| stream_error("stop prepared renderer", error));
                let released = self.release_renderer_lease().await;
                combine_cleanup(stopped, released)?;
            }
            Stream::Active { stream, monitor } => {
                let stopped = shutdown_active(stream, monitor).await;
                let released = self.release_renderer_lease().await;
                combine_cleanup(stopped, released)?;
            }
        }
        Ok(())
    }

    fn target(
        &self,
        stream: &RendererStream<OwnedFd>,
        generation: NonZeroU64,
    ) -> Result<DeviceMediaTarget, MediaPipelineError> {
        let render_node = stream.render_node_identity();
        Ok(DeviceMediaTarget {
            kind: DeviceMediaKind::Video,
            node_name: stream.identity().node_name.clone(),
            object_serial: stream.identity().object_serial,
            session_id: self.config.session_id.clone(),
            device_instance: self.config.device_instance.clone(),
            connector_id: self.config.connector_id,
            output_index: self.config.output_index,
            media_generation: generation,
            render_device: Some(RenderDeviceIdentity {
                major: render_node.major,
                minor: render_node.minor,
            }),
            caps: renderer_caps(stream.layout(), self.config.video_frame_rate)?,
        })
    }

    fn restore_renderer(&mut self, owner: OwnedFd) -> Result<(), MediaPipelineError> {
        if self.renderer.is_some() {
            return Err(MediaPipelineError::new(
                "renderer descriptor owner is already present",
            ));
        }
        self.renderer = Some(Renderer::from_fd(owner).map_err(|error| {
            MediaPipelineError::new(format!("restore renderer descriptor owner: {error}"))
        })?);
        Ok(())
    }

    fn recover_stream_error(
        &mut self,
        operation: &str,
        error: RendererStreamError<OwnedFd>,
    ) -> (MediaPipelineError, EndpointRecovery) {
        let (owner, cause) = error.into_parts();
        let recovery = owner.map(|owner| self.restore_renderer(owner));
        match recovery {
            Some(Ok(())) => (
                MediaPipelineError::new(format!("{operation}: {cause}")),
                EndpointRecovery::Retained,
            ),
            Some(Err(recovery)) => (
                MediaPipelineError::new(format!(
                    "{operation}: {cause}; renderer recovery failed: {recovery}"
                )),
                EndpointRecovery::Closed,
            ),
            None => (
                MediaPipelineError::new(format!("{operation}: {cause}")),
                EndpointRecovery::Closed,
            ),
        }
    }

    async fn finish_stream_error(
        &mut self,
        operation: &str,
        error: RendererStreamError<OwnedFd>,
    ) -> MediaPipelineError {
        let (primary, recovery) = self.recover_stream_error(operation, error);
        match recovery {
            EndpointRecovery::Retained => primary,
            EndpointRecovery::Closed => {
                combine_cleanup(Err(primary), self.release_renderer_lease().await)
                    .expect_err("a renderer stream failure remains an error")
            }
        }
    }

    async fn release_renderer_lease(&mut self) -> Result<(), MediaPipelineError> {
        self.renderer_lease
            .take()
            .expect("renderer generation owns its endpoint lifetime")
            .release()
            .await
            .map_err(|error| MediaPipelineError::new(format!("release renderer endpoint: {error}")))
    }
}

impl std::fmt::Debug for RendererCapturePipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererCapturePipeline")
            .field("config", &self.config)
            .field(
                "generation",
                &self.generation.as_ref().map(|generation| generation.id),
            )
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CapturePipelinePort for RendererCapturePipeline {
    async fn start(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<PreparedCaptureMedia, MediaPipelineError> {
        if self.generation.is_some() {
            return Err(MediaPipelineError::new(
                "a previous renderer generation still requires cleanup",
            ));
        }
        if cancellation.is_cancelled() {
            return Err(MediaPipelineError::new("renderer start was cancelled"));
        }
        let generation = NonZeroU64::new(request.media_generation)
            .ok_or_else(|| MediaPipelineError::new("media generation must be nonzero"))?;
        if self.renderer.is_none() {
            if let Some(lease) = self.renderer_lease.take() {
                lease.release().await.map_err(|error| {
                    MediaPipelineError::new(format!(
                        "release unavailable renderer endpoint: {error}"
                    ))
                })?;
            }
            let access = self
                .renderer_session
                .acquire(cancellation.clone())
                .await
                .map_err(|error| {
                    MediaPipelineError::new(format!("acquire renderer endpoint: {error}"))
                })?;
            let OpenRenderer {
                renderer,
                lease,
                render_node,
                session,
            } = access.open().map_err(|error| {
                MediaPipelineError::new(format!("open renderer endpoint: {error}"))
            })?;
            if render_node != self.render_node {
                drop(renderer);
                let rejected = Err(MediaPipelineError::new(
                    "replacement renderer selected a different GPU",
                ));
                let released = lease.release().await.map_err(|error| {
                    MediaPipelineError::new(format!(
                        "release mismatched renderer endpoint: {error}"
                    ))
                });
                return Err(combine_cleanup(rejected, released)
                    .expect_err("a rejected renderer endpoint remains an error"));
            }
            self.renderer = Some(renderer);
            self.renderer_lease = Some(lease);
            self.renderer_session = session;
        }
        let render_node = self.render_node.clone();
        let mut device_task = tokio::task::spawn_blocking(move || Device::open(render_node));
        let device = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                let _ = (&mut device_task).await;
                return Err(MediaPipelineError::new("renderer start was cancelled"));
            }
            result = &mut device_task => result
                .map_err(|error| MediaPipelineError::new(format!("join renderer GPU setup: {error}")))?
                .map_err(|error| MediaPipelineError::new(format!("open renderer GPU: {error}")))?,
        };
        let remote = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(MediaPipelineError::new("renderer start was cancelled"));
            }
            result = self.producer_remotes.create_producer_remote() => result.map_err(|error| {
                MediaPipelineError::new(format!("connect renderer PipeWire producer: {error}"))
            })?,
        };
        let renderer = self
            .renderer
            .take()
            .expect("checked renderer descriptor owner");
        let preparation = RendererStream::prepare(
            renderer,
            device,
            RendererStreamConfig {
                output_width: NonZeroU32::new(request.route.mode.width)
                    .ok_or_else(|| MediaPipelineError::new("renderer output width is zero"))?,
                output_height: NonZeroU32::new(request.route.mode.height)
                    .ok_or_else(|| MediaPipelineError::new("renderer output height is zero"))?,
                pipewire: VideoSourceConfig {
                    node_name: format!("pronk.video.{}.{generation}", self.config.session_id),
                    node_description: self.config.node_description.clone(),
                    session_id: self.config.session_id.clone(),
                    device_instance: self.config.device_instance.clone(),
                    connector_id: self.config.connector_id,
                    output_index: self.config.output_index,
                    media_generation: generation,
                    frame_rate: self.config.video_frame_rate,
                },
                output_modifier: self.config.output_modifier,
                private_capacity: self.config.private_capacity,
                output_capacity: self.config.output_capacity,
            },
            remote.into_remote(),
            cancellation.clone(),
        );
        let stream = match preparation.await {
            Ok(stream) => stream,
            Err(error) => {
                return Err(self
                    .finish_stream_error("prepare renderer stream", error)
                    .await)
            }
        };
        if cancellation.is_cancelled() {
            let cancelled = Err(MediaPipelineError::new("renderer start was cancelled"));
            let stopped = stream
                .shutdown()
                .await
                .map_err(|error| stream_error("cancel renderer stream", error));
            let released = self.release_renderer_lease().await;
            return Err(
                combine_cleanup(combine_cleanup(cancelled, stopped), released)
                    .expect_err("a cancelled renderer start remains an error"),
            );
        }
        let layout = stream.layout();
        if layout.width.get() != request.route.mode.width
            || layout.height.get() != request.route.mode.height
        {
            let actual = (layout.width, layout.height);
            let rejected = Err(MediaPipelineError::new(format!(
                "renderer output is {}x{}; active route is {}x{}",
                actual.0, actual.1, request.route.mode.width, request.route.mode.height
            )));
            let stopped = stream
                .shutdown()
                .await
                .map_err(|error| stream_error("stop mismatched renderer stream", error));
            let released = self.release_renderer_lease().await;
            return Err(
                combine_cleanup(combine_cleanup(rejected, stopped), released)
                    .expect_err("a mismatched renderer remains an error"),
            );
        }
        let target = match self.target(&stream, generation) {
            Ok(target) => target,
            Err(error) => {
                let stopped = stream
                    .shutdown()
                    .await
                    .map_err(|shutdown| stream_error("stop rejected renderer stream", shutdown));
                let released = self.release_renderer_lease().await;
                return Err(
                    combine_cleanup(combine_cleanup(Err(error), stopped), released)
                        .expect_err("a rejected renderer target remains an error"),
                );
            }
        };
        self.generation = Some(Generation {
            id: generation,
            stream: Stream::Prepared(stream),
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
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        let Some(generation) = self.generation.take() else {
            return Err(MediaPipelineError::new(
                "no renderer generation is prepared",
            ));
        };
        if generation.id != media_generation {
            let actual = generation.id;
            self.generation = Some(generation);
            return Err(MediaPipelineError::new(format!(
                "renderer activation requested generation {media_generation}; prepared generation is {actual}"
            )));
        }
        match generation.stream {
            Stream::Active { stream, monitor } => {
                let error = match stream.state() {
                    RendererStreamState::Active => None,
                    RendererStreamState::Failed(error) => Some(error),
                    state => Some(format!(
                        "renderer generation has invalid active state {state:?}"
                    )),
                };
                self.generation = Some(Generation {
                    id: generation.id,
                    stream: Stream::Active { stream, monitor },
                });
                error.map_or(Ok(()), |error| Err(MediaPipelineError::new(error)))
            }
            Stream::Prepared(stream) => {
                let state = stream.subscribe();
                let stream = match stream.activate(cancellation).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        return Err(self
                            .finish_stream_error("activate renderer stream", error)
                            .await);
                    }
                };
                let monitor = monitor_active_renderer(generation.id, state, self.events.clone());
                self.generation = Some(Generation {
                    id: generation.id,
                    stream: Stream::Active { stream, monitor },
                });
                Ok(())
            }
        }
    }

    async fn suspend(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaSuspendReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.stop_generation(media_generation, cancellation).await
    }

    async fn stop(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.stop_generation(media_generation, cancellation).await
    }

    async fn shutdown(
        &mut self,
        _reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        if let Some(generation) = self.generation.as_ref().map(|generation| generation.id) {
            self.stop_generation(generation, cancellation).await?;
        }
        self.renderer.take();
        if let Some(lease) = self.renderer_lease.take() {
            lease.release().await.map_err(|error| {
                MediaPipelineError::new(format!("release idle renderer endpoint: {error}"))
            })?;
        }
        Ok(())
    }
}

fn combine_cleanup(
    primary: Result<(), MediaPipelineError>,
    cleanup: Result<(), MediaPipelineError>,
) -> Result<(), MediaPipelineError> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(MediaPipelineError::new(format!(
            "{primary}; cleanup also failed: {cleanup}"
        ))),
    }
}

async fn shutdown_active(
    stream: ActiveRendererStream<OwnedFd>,
    monitor: CaptureMonitor,
) -> Result<(), MediaPipelineError> {
    monitor.cancel();
    let (stream, monitor) = tokio::join!(stream.shutdown(), monitor.shutdown());
    let stream = stream.map_err(|error| stream_error("stop active renderer", error));
    match (stream, monitor) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(stream), Err(monitor)) => Err(MediaPipelineError::new(format!(
            "{stream}; active renderer monitor cleanup also failed: {monitor}"
        ))),
    }
}

fn monitor_active_renderer(
    media_generation: NonZeroU64,
    state: tokio::sync::watch::Receiver<RendererStreamState>,
    events: mpsc::UnboundedSender<CaptureEvent>,
) -> CaptureMonitor {
    CaptureMonitor::watch(
        media_generation,
        state,
        events,
        |state| match state {
            RendererStreamState::Failed(error) => Some(error.clone()),
            RendererStreamState::Stopped => Some("renderer stream stopped unexpectedly".into()),
            RendererStreamState::Prepared | RendererStreamState::Active => None,
        },
        "renderer stream health channel closed",
    )
}

fn renderer_caps(
    layout: VideoBufferLayout,
    frame_rate: VideoFrameRate,
) -> Result<String, MediaPipelineError> {
    let fourcc = match layout.format {
        VideoPixelFormat::Xrgb8888 => "XR24",
        VideoPixelFormat::Argb8888 => "AR24",
    };
    let VideoBufferStorage::DrmModifier { modifier, .. } = layout.storage else {
        return Err(MediaPipelineError::new(
            "renderer output does not have a DRM modifier",
        ));
    };
    let drm_format = if modifier == 0 {
        fourcc.to_string()
    } else {
        format!("{fourcc}:0x{modifier:016x}")
    };
    Ok(format!(
        "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm_format},width={},height={},framerate={}/{}",
        layout.width,
        layout.height,
        frame_rate.numerator(),
        frame_rate.denominator()
    ))
}

fn stream_error<F>(operation: &str, error: RendererStreamError<F>) -> MediaPipelineError {
    let (_, error) = error.into_parts();
    MediaPipelineError::new(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_caps_describe_the_registered_gpu_layout() {
        let caps = renderer_caps(
            VideoBufferLayout {
                format: VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(1920).unwrap(),
                height: NonZeroU32::new(1080).unwrap(),
                pitch: NonZeroU32::new(7680).unwrap(),
                size: NonZeroU64::new(8_294_400).unwrap(),
                storage: VideoBufferStorage::DrmModifier {
                    modifier: 0x100000000000001,
                    offset: 0,
                },
            },
            VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
        )
        .unwrap();
        assert_eq!(
            caps,
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=XR24:0x0100000000000001,width=1920,height=1080,framerate=30/1"
        );
    }

    #[test]
    fn linear_modifier_caps_use_the_plain_drm_format() {
        let caps = renderer_caps(
            VideoBufferLayout {
                format: VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(64).unwrap(),
                height: NonZeroU32::new(32).unwrap(),
                pitch: NonZeroU32::new(256).unwrap(),
                size: NonZeroU64::new(8192).unwrap(),
                storage: VideoBufferStorage::DrmModifier {
                    modifier: 0,
                    offset: 0,
                },
            },
            VideoFrameRate::integer(NonZeroU32::new(60).unwrap()),
        )
        .unwrap();
        assert!(caps.contains("drm-format=XR24,"));
        assert!(caps.ends_with("framerate=60/1"));
    }

    #[test]
    fn renderer_caps_preserve_a_fractional_frame_rate() {
        let caps = renderer_caps(
            VideoBufferLayout {
                format: VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(1920).unwrap(),
                height: NonZeroU32::new(1080).unwrap(),
                pitch: NonZeroU32::new(7680).unwrap(),
                size: NonZeroU64::new(8_294_400).unwrap(),
                storage: VideoBufferStorage::DrmModifier {
                    modifier: 0,
                    offset: 0,
                },
            },
            VideoFrameRate::new(
                NonZeroU32::new(30_000).unwrap(),
                NonZeroU32::new(1_001).unwrap(),
            ),
        )
        .unwrap();

        assert!(caps.ends_with("framerate=30000/1001"));
    }

    #[tokio::test]
    async fn active_monitor_reports_the_exact_failed_generation() {
        let generation = NonZeroU64::new(7).unwrap();
        let (state, receive) = tokio::sync::watch::channel(RendererStreamState::Active);
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let monitor = monitor_active_renderer(generation, receive, events);
        state.send_replace(RendererStreamState::Failed("device lost".into()));
        assert_eq!(
            event_rx.recv().await,
            Some(CaptureEvent::Failed {
                media_generation: generation,
                error: "device lost".into(),
            })
        );
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn orderly_monitor_shutdown_does_not_report_failure() {
        let (state, receive) = tokio::sync::watch::channel(RendererStreamState::Active);
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let monitor = monitor_active_renderer(NonZeroU64::new(9).unwrap(), receive, events);
        monitor.cancel();
        state.send_replace(RendererStreamState::Stopped);
        monitor.shutdown().await.unwrap();
        assert_eq!(event_rx.recv().await, None);
    }
}
