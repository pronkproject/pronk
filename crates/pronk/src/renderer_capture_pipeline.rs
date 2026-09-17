//! Userspace rendering joined to generic final-image capture.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use castkms_renderer::Renderer;
use drm_capture::Access as CaptureAccess;
use pronk_capture::Layout;
use pronk_capture_pipewire::{State as CaptureVideoState, Video as CaptureVideo};
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{ClassifiedSocketRemoteProvider, VideoFrameRate, VideoSourceConfig};
use pronk_renderer_service::{
    ActiveRendererStream, PrivatePoolConfig, RendererStream, RendererStreamConfig,
    RendererStreamError, RendererStreamState,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::capability_lease::CapabilityLease;
use crate::capture_health::{CaptureEvents, CaptureMonitor};
use crate::device_session_port::{
    DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaTarget, RenderDeviceIdentity,
};
use crate::drm_capture_pipeline::{capture_caps, CaptureOwner, CaptureSetup, CaptureSetupConfig};
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
    pub private_pool: RendererPrivatePoolConfig,
    pub capture_pool_size: NonZeroU32,
    pub capture_request_capacity: NonZeroU32,
    pub capture_pool_byte_limit: NonZeroU64,
    pub capture_heap_path: PathBuf,
    pub capture_poll_interval: Duration,
    pub capture_shutdown_timeout: Duration,
}

/// Allocation policy for renderer-private scene storage.
#[derive(Debug, Clone, Copy)]
pub struct RendererPrivatePoolConfig {
    pub modifier: u64,
    pub frame_capacity: NonZeroUsize,
    pub source_capacity: NonZeroUsize,
}

type Video = CaptureVideo<CaptureOwner>;

enum Stream {
    Prepared {
        renderer: RendererStream<OwnedFd>,
        video: Video,
    },
    Active {
        renderer: ActiveRendererStream<OwnedFd>,
        video: Video,
        monitors: Monitors,
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

struct Monitors {
    renderer: CaptureMonitor,
    capture: CaptureMonitor,
}

/// Sole owner of renderer authority and its generic capture producer.
pub struct RendererCapturePipeline {
    renderer: Option<Renderer<OwnedFd>>,
    render_node: PathBuf,
    renderer_session: RendererSession,
    capture_setup: CaptureSetup,
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
        capture: CaptureAccess,
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
                capture_setup: CaptureSetup::new(capture),
                producer_remotes,
                config,
                generation: None,
                events,
            },
            receive,
        ))
    }

    fn capture_config(&self) -> CaptureSetupConfig {
        CaptureSetupConfig {
            pool_size: self.config.capture_pool_size,
            request_capacity: self.config.capture_request_capacity,
            pool_byte_limit: self.config.capture_pool_byte_limit,
            heap_path: self.config.capture_heap_path.clone(),
            poll_interval: self.config.capture_poll_interval,
            shutdown_timeout: self.config.capture_shutdown_timeout,
        }
    }

    async fn stop_generation(&mut self, id: NonZeroU64) -> Result<(), MediaPipelineError> {
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
        let stopped = match generation.stream {
            Stream::Prepared { renderer, video } => shutdown_prepared(renderer, video).await,
            Stream::Active {
                renderer,
                video,
                monitors,
            } => shutdown_active(renderer, video, monitors).await,
        };
        let released = self.release_renderer_lease().await;
        combine_cleanup(stopped, released)
    }

    fn target(
        &self,
        renderer: &RendererStream<OwnedFd>,
        video: &Video,
        layout: Layout,
        generation: NonZeroU64,
    ) -> DeviceMediaTarget {
        let render_node = renderer.render_node_identity();
        DeviceMediaTarget {
            kind: DeviceMediaKind::Video,
            node_name: video.identity().node_name.clone(),
            object_serial: video.identity().object_serial,
            session_id: self.config.session_id.clone(),
            device_instance: self.config.device_instance.clone(),
            connector_id: self.config.connector_id,
            output_index: self.config.output_index,
            media_generation: generation,
            render_device: Some(RenderDeviceIdentity {
                major: render_node.major,
                minor: render_node.minor,
            }),
            caps: capture_caps(layout, self.config.video_frame_rate),
        }
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

    async fn finish_prepared_renderer(
        &mut self,
        renderer: RendererStream<OwnedFd>,
        primary: MediaPipelineError,
    ) -> MediaPipelineError {
        let stopped = renderer
            .shutdown()
            .await
            .map_err(|error| stream_error("stop prepared renderer", error));
        let released = self.release_renderer_lease().await;
        combine_cleanup(combine_cleanup(Err(primary), stopped), released)
            .expect_err("a renderer setup failure remains an error")
    }

    async fn wait_for_capture_offer_change(
        &self,
        previous: drm_capture::OfferId,
        mut renderer_state: tokio::sync::watch::Receiver<RendererStreamState>,
        cancellation: CancellationToken,
    ) -> Result<drm_capture::Description, MediaPipelineError> {
        loop {
            require_running_renderer(&renderer_state)?;
            let description = self.capture_setup.describe(cancellation.clone()).await?;
            if description.offer != previous {
                return Ok(description);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(MediaPipelineError::new("renderer start was cancelled"));
                }
                changed = renderer_state.changed() => {
                    changed.map_err(|_| {
                        MediaPipelineError::new("renderer health channel closed during setup")
                    })?;
                }
                _ = tokio::time::sleep(self.config.capture_poll_interval) => {}
            }
        }
    }

    async fn ensure_renderer(
        &mut self,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        if self.renderer.is_some() {
            return Ok(());
        }
        if let Some(lease) = self.renderer_lease.take() {
            lease.release().await.map_err(|error| {
                MediaPipelineError::new(format!("release unavailable renderer endpoint: {error}"))
            })?;
        }
        let access = self
            .renderer_session
            .acquire(cancellation)
            .await
            .map_err(|error| {
                MediaPipelineError::new(format!("acquire renderer endpoint: {error}"))
            })?;
        let OpenRenderer {
            renderer,
            lease,
            render_node,
            session,
        } = access
            .open()
            .map_err(|error| MediaPipelineError::new(format!("open renderer endpoint: {error}")))?;
        if render_node != self.render_node {
            drop(renderer);
            let rejected = Err(MediaPipelineError::new(
                "replacement renderer selected a different GPU",
            ));
            let released = lease.release().await.map_err(|error| {
                MediaPipelineError::new(format!("release mismatched renderer endpoint: {error}"))
            });
            return combine_cleanup(rejected, released);
        }
        self.renderer = Some(renderer);
        self.renderer_lease = Some(lease);
        self.renderer_session = session;
        Ok(())
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
        self.capture_config().validate()?;
        check_cancellation(&cancellation, "renderer start was cancelled")?;
        let generation = NonZeroU64::new(request.media_generation)
            .ok_or_else(|| MediaPipelineError::new("media generation must be nonzero"))?;
        self.ensure_renderer(cancellation.clone()).await?;
        let interval = self
            .config
            .video_frame_rate
            .frame_interval()
            .ok_or_else(|| {
                MediaPipelineError::new("renderer cadence exceeds the source clock resolution")
            })?;
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
        let previous_offer = self
            .capture_setup
            .describe(cancellation.clone())
            .await?
            .offer;
        let output_width = NonZeroU32::new(request.route.mode.width)
            .ok_or_else(|| MediaPipelineError::new("renderer output width is zero"))?;
        let output_height = NonZeroU32::new(request.route.mode.height)
            .ok_or_else(|| MediaPipelineError::new("renderer output height is zero"))?;
        let renderer = self
            .renderer
            .take()
            .expect("checked renderer descriptor owner");
        let preparation = RendererStream::prepare(
            renderer,
            device,
            RendererStreamConfig {
                output_width,
                output_height,
                source_interval: interval,
                private_pool: PrivatePoolConfig {
                    modifier: self.config.private_pool.modifier,
                    frame_capacity: self.config.private_pool.frame_capacity,
                    source_capacity: self.config.private_pool.source_capacity,
                },
            },
            cancellation.clone(),
        );
        let renderer = match preparation.await {
            Ok(renderer) => renderer,
            Err(error) => {
                return Err(self
                    .finish_stream_error("prepare renderer stream", error)
                    .await);
            }
        };
        let renderer_state = renderer.subscribe();
        let selected = match self
            .wait_for_capture_offer_change(previous_offer, renderer_state, cancellation.clone())
            .await
        {
            Ok(selected) => selected,
            Err(error) => return Err(self.finish_prepared_renderer(renderer, error).await),
        };
        let (actor, layout) = match self
            .capture_setup
            .create_actor(
                self.capture_config(),
                request,
                Some(selected.offer),
                cancellation.clone(),
            )
            .await
        {
            Ok(created) => created,
            Err(error) => return Err(self.finish_prepared_renderer(renderer, error).await),
        };
        if renderer.output().width() != layout.width.get()
            || renderer.output().height() != layout.height.get()
        {
            return Err(self
                .finish_prepared_renderer(
                    renderer,
                    MediaPipelineError::new(
                        "renderer output does not match its capture destination",
                    ),
                )
                .await);
        }
        let remote = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(self.finish_prepared_renderer(
                    renderer,
                    MediaPipelineError::new("renderer start was cancelled"),
                ).await);
            }
            result = self.producer_remotes.create_producer_remote() => match result {
                Ok(remote) => remote,
                Err(error) => {
                    return Err(self.finish_prepared_renderer(
                        renderer,
                        MediaPipelineError::new(format!("connect capture PipeWire producer: {error}")),
                    ).await);
                }
            },
        };
        let video = match CaptureVideo::prepare(
            actor,
            VideoSourceConfig {
                node_name: format!("pronk.video.{}.{generation}", self.config.session_id),
                node_description: self.config.node_description.clone(),
                session_id: self.config.session_id.clone(),
                device_instance: self.config.device_instance.clone(),
                connector_id: self.config.connector_id,
                output_index: self.config.output_index,
                media_generation: generation,
                frame_rate: self.config.video_frame_rate,
            },
            remote.into_remote(),
        )
        .await
        {
            Ok(video) => video,
            Err(error) => {
                return Err(self
                    .finish_prepared_renderer(
                        renderer,
                        MediaPipelineError::new(format!("prepare capture video: {error}")),
                    )
                    .await);
            }
        };
        if cancellation.is_cancelled() {
            let cancelled = Err(MediaPipelineError::new("renderer start was cancelled"));
            let stopped = shutdown_prepared(renderer, video).await;
            let released = self.release_renderer_lease().await;
            return Err(
                combine_cleanup(combine_cleanup(cancelled, stopped), released)
                    .expect_err("a cancelled renderer start remains an error"),
            );
        }
        let target = self.target(&renderer, &video, layout, generation);
        self.generation = Some(Generation {
            id: generation,
            stream: Stream::Prepared { renderer, video },
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
            Stream::Active {
                renderer,
                video,
                monitors,
            } => {
                let result = match renderer.state() {
                    RendererStreamState::Active => video.activate().await.map_err(|error| {
                        MediaPipelineError::new(format!("resume capture video: {error}"))
                    }),
                    RendererStreamState::Failed(error) => Err(MediaPipelineError::new(error)),
                    state => Err(MediaPipelineError::new(format!(
                        "renderer generation has invalid active state {state:?}"
                    ))),
                };
                self.generation = Some(Generation {
                    id: generation.id,
                    stream: Stream::Active {
                        renderer,
                        video,
                        monitors,
                    },
                });
                result
            }
            Stream::Prepared { renderer, video } => {
                let renderer_state = renderer.subscribe();
                let renderer = match renderer.activate(cancellation).await {
                    Ok(renderer) => renderer,
                    Err(error) => {
                        let primary = self
                            .finish_stream_error("activate renderer stream", error)
                            .await;
                        let stopped = shutdown_video(video).await;
                        return Err(combine_cleanup(Err(primary), stopped)
                            .expect_err("renderer activation failure remains an error"));
                    }
                };
                if let Err(error) = video.activate().await {
                    let primary = Err(MediaPipelineError::new(format!(
                        "activate capture video: {error}"
                    )));
                    let stopped = shutdown_active_without_monitors(renderer, video).await;
                    let released = self.release_renderer_lease().await;
                    return Err(combine_cleanup(combine_cleanup(primary, stopped), released)
                        .expect_err("capture activation failure remains an error"));
                }
                let monitors = Monitors {
                    renderer: monitor_active_renderer(
                        generation.id,
                        renderer_state,
                        self.events.clone(),
                    ),
                    capture: monitor_capture_video(
                        generation.id,
                        video.subscribe(),
                        self.events.clone(),
                    ),
                };
                self.generation = Some(Generation {
                    id: generation.id,
                    stream: Stream::Active {
                        renderer,
                        video,
                        monitors,
                    },
                });
                Ok(())
            }
        }
    }

    async fn suspend(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaSuspendReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        let generation = self
            .generation
            .as_ref()
            .ok_or_else(|| MediaPipelineError::new("no renderer generation is active"))?;
        if generation.id != media_generation {
            return Err(MediaPipelineError::new(format!(
                "renderer suspension requested generation {media_generation}; active generation is {}",
                generation.id
            )));
        }
        match &generation.stream {
            Stream::Prepared { .. } => Ok(()),
            Stream::Active { video, .. } => video.suspend().await.map_err(|error| {
                MediaPipelineError::new(format!("suspend capture video: {error}"))
            }),
        }
    }

    async fn stop(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaStopReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.stop_generation(media_generation).await
    }

    async fn shutdown(
        &mut self,
        _reason: MediaStopReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        if let Some(generation) = self.generation.as_ref().map(|generation| generation.id) {
            self.stop_generation(generation).await?;
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

impl Monitors {
    fn cancel(&self) {
        self.renderer.cancel();
        self.capture.cancel();
    }

    async fn shutdown(self) -> Result<(), MediaPipelineError> {
        let (renderer, capture) = tokio::join!(self.renderer.shutdown(), self.capture.shutdown());
        combine_cleanup(renderer, capture)
    }
}

fn check_cancellation(
    cancellation: &CancellationToken,
    message: &'static str,
) -> Result<(), MediaPipelineError> {
    if cancellation.is_cancelled() {
        Err(MediaPipelineError::new(message))
    } else {
        Ok(())
    }
}

fn require_running_renderer(
    state: &tokio::sync::watch::Receiver<RendererStreamState>,
) -> Result<(), MediaPipelineError> {
    match state.borrow().clone() {
        RendererStreamState::Prepared | RendererStreamState::Active => Ok(()),
        RendererStreamState::Failed(error) => Err(MediaPipelineError::new(format!(
            "renderer failed while awaiting constraints selection: {error}"
        ))),
        RendererStreamState::Stopped => Err(MediaPipelineError::new(
            "renderer stopped while awaiting constraints selection",
        )),
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

async fn shutdown_video(video: Video) -> Result<(), MediaPipelineError> {
    video
        .shutdown()
        .await
        .map(drop)
        .map_err(|error| MediaPipelineError::new(format!("stop capture video: {error}")))
}

async fn shutdown_prepared(
    renderer: RendererStream<OwnedFd>,
    video: Video,
) -> Result<(), MediaPipelineError> {
    let (renderer, video) = tokio::join!(renderer.shutdown(), shutdown_video(video));
    let renderer = renderer.map_err(|error| stream_error("stop prepared renderer", error));
    combine_cleanup(renderer, video)
}

async fn shutdown_active_without_monitors(
    renderer: ActiveRendererStream<OwnedFd>,
    video: Video,
) -> Result<(), MediaPipelineError> {
    let (renderer, video) = tokio::join!(renderer.shutdown(), shutdown_video(video));
    let renderer = renderer.map_err(|error| stream_error("stop active renderer", error));
    combine_cleanup(renderer, video)
}

async fn shutdown_active(
    renderer: ActiveRendererStream<OwnedFd>,
    video: Video,
    monitors: Monitors,
) -> Result<(), MediaPipelineError> {
    monitors.cancel();
    let (streams, monitors) = tokio::join!(
        shutdown_active_without_monitors(renderer, video),
        monitors.shutdown()
    );
    combine_cleanup(streams, monitors)
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

fn monitor_capture_video(
    media_generation: NonZeroU64,
    state: tokio::sync::watch::Receiver<CaptureVideoState>,
    events: mpsc::UnboundedSender<CaptureEvent>,
) -> CaptureMonitor {
    CaptureMonitor::watch(
        media_generation,
        state,
        events,
        |state| match state {
            CaptureVideoState::Active => None,
            CaptureVideoState::Failed(error) => Some(error.clone()),
            CaptureVideoState::Stopped => Some("capture video stopped unexpectedly".into()),
        },
        "capture video health channel closed",
    )
}

fn stream_error<F>(operation: &str, error: RendererStreamError<F>) -> MediaPipelineError {
    let (_, error) = error.into_parts();
    MediaPipelineError::new(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_wait_rejects_a_renderer_that_already_failed() {
        let (_, state) =
            tokio::sync::watch::channel(RendererStreamState::Failed("native device lost".into()));
        assert!(require_running_renderer(&state)
            .unwrap_err()
            .to_string()
            .contains("native device lost"));
    }

    #[tokio::test]
    async fn active_renderer_monitor_reports_the_exact_generation() {
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
    async fn capture_monitor_reports_the_exact_generation() {
        let generation = NonZeroU64::new(8).unwrap();
        let (state, receive) = tokio::sync::watch::channel(CaptureVideoState::Active);
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let monitor = monitor_capture_video(generation, receive, events);
        state.send_replace(CaptureVideoState::Failed("grant revoked".into()));
        assert_eq!(
            event_rx.recv().await,
            Some(CaptureEvent::Failed {
                media_generation: generation,
                error: "grant revoked".into(),
            })
        );
        monitor.shutdown().await.unwrap();
    }
}
