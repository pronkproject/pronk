//! Userspace rendering joined to generic final-image capture.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use castkms_renderer::Renderer;
use drm_capture::Access as CaptureAccess;
use pronk_backend_protocol::{RawVideoLayout, RawVideoStorage};
use pronk_capture::Buffer;
use pronk_capture_pipewire::{State as CaptureVideoState, Video as CaptureVideo};
use pronk_gpu::vulkan::{Device, PackedFormat};
use pronk_pipewire::{ClassifiedSocketRemoteProvider, VideoFrameRate, VideoSourceConfig};
use pronk_renderer_service::{
    ActiveRendererStream, PrivatePoolConfig, RendererStream, RendererStreamConfig,
    RendererStreamError, RendererStreamState,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::capability_lease::CapabilityLease;
use crate::capture_health::{CaptureEvents, CaptureMonitor};
use crate::device_session_port::{
    DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaTarget, RenderDeviceIdentity,
};
use crate::drm_capture_pipeline::{
    capture_caps, CaptureOwner, CaptureSetup, CaptureSetupConfig, DescriptionState,
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
    pub raw_layout: RawVideoLayout,
    pub video_bitrate: NonZeroU64,
    pub video_frame_rate: VideoFrameRate,
    pub private_pool: RendererPrivatePoolConfig,
    pub capture_pool_size: NonZeroU32,
    pub capture_request_capacity: NonZeroU32,
    pub capture_pool_byte_limit: NonZeroU64,
    pub capture_heap_path: PathBuf,
    pub capture_poll_interval: Duration,
    pub capture_offer_timeout: Duration,
    pub capture_shutdown_timeout: Duration,
}

/// Maximum allocation policy for renderer-private scene storage.
#[derive(Debug, Clone, Copy)]
pub struct RendererPrivatePoolConfig {
    pub modifier: Option<u64>,
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

impl Generation {
    async fn shutdown(self) -> Result<(), MediaPipelineError> {
        match self.stream {
            Stream::Prepared { renderer, video } => shutdown_prepared(renderer, video).await,
            Stream::Active {
                renderer,
                video,
                monitors,
            } => shutdown_active(renderer, video, monitors).await,
        }
    }
}

/// A running generation carries its endpoint lease across a transition.
struct TakenGeneration {
    lease: CapabilityLease,
    generation: Generation,
}

/// The renderer descriptor and its revocation lease move together.
enum PipelineState {
    Idle {
        renderer: Renderer<OwnedFd>,
        lease: CapabilityLease,
    },
    Starting {
        lease: CapabilityLease,
    },
    Running {
        lease: CapabilityLease,
        generation: Generation,
    },
    Stopping {
        generation_id: NonZeroU64,
        task: JoinHandle<Result<(), MediaPipelineError>>,
    },
    Unavailable,
}

impl PipelineState {
    fn stop_generation(taken: TakenGeneration) -> Self {
        let TakenGeneration { lease, generation } = taken;
        let generation_id = generation.id;
        let task = tokio::spawn(async move {
            let stopped = generation.shutdown().await;
            let released = lease.release().await.map_err(|error| {
                MediaPipelineError::new(format!("release renderer endpoint: {error}"))
            });
            combine_cleanup(stopped, released)
        });
        Self::Stopping {
            generation_id,
            task,
        }
    }

    async fn finish_stopping(&mut self, id: NonZeroU64) -> Result<(), MediaPipelineError> {
        let Self::Stopping {
            generation_id,
            task,
        } = self
        else {
            return Err(MediaPipelineError::new(
                "renderer generation is not stopping",
            ));
        };
        if *generation_id != id {
            return Err(MediaPipelineError::new(format!(
                "renderer stop requested generation {id}; stopping generation is {generation_id}"
            )));
        }
        let result = task.await.map_err(|error| {
            MediaPipelineError::new(format!("renderer generation cleanup task failed: {error}"))
        });
        *self = Self::Unavailable;
        result?
    }

    async fn release_lease(&mut self) -> Result<(), MediaPipelineError> {
        let previous = std::mem::replace(self, Self::Unavailable);
        let lease = match previous {
            Self::Idle { lease, .. } | Self::Starting { lease } => lease,
            still_active @ (Self::Running { .. } | Self::Stopping { .. }) => {
                *self = still_active;
                return Err(MediaPipelineError::new(
                    "renderer generation must stop before its endpoint is released",
                ));
            }
            Self::Unavailable => {
                return Err(MediaPipelineError::new("renderer endpoint has no lease"));
            }
        };
        lease
            .release()
            .await
            .map_err(|error| MediaPipelineError::new(format!("release renderer endpoint: {error}")))
    }
}

struct Monitors {
    renderer: CaptureMonitor,
    capture: CaptureMonitor,
}

/// Sole owner of renderer authority and its generic capture producer.
pub struct RendererCapturePipeline {
    state: PipelineState,
    render_node: PathBuf,
    renderer_session: RendererSession,
    capture_setup: CaptureSetup,
    producer_remotes: ClassifiedSocketRemoteProvider,
    config: RendererCapturePipelineConfig,
    events: mpsc::UnboundedSender<CaptureEvent>,
}

impl RendererCapturePipeline {
    fn generation(&self) -> Option<&Generation> {
        match &self.state {
            PipelineState::Running { generation, .. } => Some(generation),
            PipelineState::Idle { .. }
            | PipelineState::Starting { .. }
            | PipelineState::Stopping { .. }
            | PipelineState::Unavailable => None,
        }
    }

    fn take_generation(&mut self) -> Option<TakenGeneration> {
        let previous = std::mem::replace(&mut self.state, PipelineState::Unavailable);
        match previous {
            PipelineState::Running { lease, generation } => {
                Some(TakenGeneration { lease, generation })
            }
            other => {
                self.state = other;
                None
            }
        }
    }

    fn restore_generation(&mut self, taken: TakenGeneration) {
        self.state = PipelineState::Running {
            lease: taken.lease,
            generation: taken.generation,
        };
    }

    fn take_renderer_for_start(&mut self) -> Result<Renderer<OwnedFd>, MediaPipelineError> {
        let previous = std::mem::replace(&mut self.state, PipelineState::Unavailable);
        match previous {
            PipelineState::Idle { renderer, lease } => {
                self.state = PipelineState::Starting { lease };
                Ok(renderer)
            }
            other => {
                self.state = other;
                Err(MediaPipelineError::new("renderer endpoint is not idle"))
            }
        }
    }

    async fn install_started_generation(
        &mut self,
        generation: Generation,
    ) -> Result<(), MediaPipelineError> {
        match std::mem::replace(&mut self.state, PipelineState::Unavailable) {
            PipelineState::Starting { lease } => {
                self.state = PipelineState::Running { lease, generation };
                Ok(())
            }
            other => {
                self.state = other;
                let stopped = generation.shutdown().await;
                combine_cleanup(
                    Err(MediaPipelineError::new(
                        "renderer endpoint left its starting phase",
                    )),
                    stopped,
                )
            }
        }
    }

    fn requested_capture_layout(&self) -> std::io::Result<Option<drm_capture::RequestedLayout>> {
        match self.config.raw_layout.storage {
            RawVideoStorage::SystemMemory => Ok(None),
            RawVideoStorage::DmaBuf => drm_capture::RequestedLayout::new(
                self.config.raw_layout.format,
                self.config.raw_layout.modifier,
            )
            .map(Some),
        }
    }

    pub fn new(
        access: RendererAccess,
        capture: CaptureAccess,
        producer_remotes: ClassifiedSocketRemoteProvider,
        config: RendererCapturePipelineConfig,
    ) -> std::io::Result<(Self, CaptureEvents)> {
        if config.capture_offer_timeout.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "capture offer timeout must be nonzero",
            ));
        }
        let OpenRenderer {
            renderer,
            lease,
            render_node,
            session,
        } = access.open()?;
        let (events, receive) = CaptureEvents::channel();
        Ok((
            Self {
                state: PipelineState::Idle { renderer, lease },
                render_node,
                renderer_session: session,
                capture_setup: CaptureSetup::new(capture),
                producer_remotes,
                config,
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
        if matches!(self.state, PipelineState::Stopping { .. }) {
            return self.state.finish_stopping(id).await;
        }
        if matches!(self.state, PipelineState::Starting { .. }) {
            // A cancelled start can drop its descriptor-owning future while
            // leaving the issuer lease here. There is no generation to stop,
            // but the endpoint still needs explicit release before retry.
            return self.release_renderer_lease().await;
        }
        let Some(taken) = self.take_generation() else {
            return Ok(());
        };
        if taken.generation.id != id {
            let actual = taken.generation.id;
            self.restore_generation(taken);
            return Err(MediaPipelineError::new(format!(
                "renderer stop requested generation {id}; active generation is {actual}"
            )));
        }
        self.state = PipelineState::stop_generation(taken);
        self.state.finish_stopping(id).await
    }

    fn target(
        &self,
        renderer: &RendererStream<OwnedFd>,
        video: &Video,
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
            caps: capture_caps(video.layout(), self.config.video_frame_rate),
        }
    }

    fn restore_renderer(&mut self, owner: OwnedFd) -> Result<(), MediaPipelineError> {
        let renderer = Renderer::from_fd(owner).map_err(|error| {
            MediaPipelineError::new(format!("restore renderer descriptor owner: {error}"))
        })?;
        let previous = std::mem::replace(&mut self.state, PipelineState::Unavailable);
        let PipelineState::Starting { lease } = previous else {
            self.state = previous;
            return Err(MediaPipelineError::new(
                "renderer descriptor owner is already present",
            ));
        };
        self.state = PipelineState::Idle { renderer, lease };
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
        tracing::warn!(error = %primary, "renderer stream setup failed");
        match recovery {
            EndpointRecovery::Retained => primary,
            EndpointRecovery::Closed => {
                combine_cleanup(Err(primary), self.release_renderer_lease().await)
                    .expect_err("a renderer stream failure remains an error")
            }
        }
    }

    async fn release_renderer_lease(&mut self) -> Result<(), MediaPipelineError> {
        self.state.release_lease().await
    }

    async fn finish_prepared_renderer(
        &mut self,
        renderer: RendererStream<OwnedFd>,
        primary: MediaPipelineError,
    ) -> MediaPipelineError {
        tracing::warn!(error = %primary, "renderer capture setup failed");
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
        previous: Option<drm_capture::OfferId>,
        mut renderer_state: tokio::sync::watch::Receiver<RendererStreamState>,
        cancellation: CancellationToken,
    ) -> Result<drm_capture::Description, MediaPipelineError> {
        let deadline = tokio::time::Instant::now() + self.config.capture_offer_timeout;
        loop {
            require_running_renderer(&renderer_state)?;
            if let DescriptionState::Active(description) = self
                .capture_setup
                .describe_if_active(
                    self.requested_capture_layout().map_err(|error| {
                        MediaPipelineError::new(format!("select capture layout: {error}"))
                    })?,
                    cancellation.clone(),
                )
                .await?
            {
                if previous.is_none_or(|previous| description.offer != previous) {
                    require_running_renderer(&renderer_state)?;
                    return Ok(description);
                }
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(MediaPipelineError::new("renderer start was cancelled"));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(MediaPipelineError::new(
                        "capture output did not accept the negotiated layout before the renderer deadline"
                    ));
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
        match self.state {
            PipelineState::Idle { .. } => return Ok(()),
            PipelineState::Starting { .. }
            | PipelineState::Running { .. }
            | PipelineState::Stopping { .. } => {
                return Err(MediaPipelineError::new(
                    "a previous renderer generation still requires cleanup",
                ));
            }
            PipelineState::Unavailable => {}
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
        self.state = PipelineState::Idle { renderer, lease };
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
                &self.generation().map(|generation| generation.id),
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
        if self.generation().is_some() {
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
        let capture_device = device.clone();
        let requested_layout = self
            .requested_capture_layout()
            .map_err(|error| MediaPipelineError::new(format!("select capture layout: {error}")))?;
        let previous_offer = match self
            .capture_setup
            .describe_if_active(requested_layout, cancellation.clone())
            .await?
        {
            DescriptionState::Active(description) => Some(description.offer),
            DescriptionState::Inactive | DescriptionState::UnsupportedLayout => None,
        };
        let output_width = NonZeroU32::new(request.route.mode.width)
            .ok_or_else(|| MediaPipelineError::new("renderer output width is zero"))?;
        let output_height = NonZeroU32::new(request.route.mode.height)
            .ok_or_else(|| MediaPipelineError::new("renderer output height is zero"))?;
        let output_format = capture_format(self.config.raw_layout.format)?;
        let renderer = self.take_renderer_for_start()?;
        let preparation = RendererStream::prepare(
            renderer,
            device,
            RendererStreamConfig {
                output_width,
                output_height,
                output_format,
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
        if selected.width.get() != request.route.mode.width
            || selected.height.get() != request.route.mode.height
            || selected.refresh_millihz.get() != request.route.mode.refresh_millihz
        {
            return Err(self
                .finish_prepared_renderer(
                    renderer,
                    MediaPipelineError::new(format!(
                        "capture output is {}x{} at {} mHz; active route is {}x{} at {} mHz",
                        selected.width,
                        selected.height,
                        selected.refresh_millihz,
                        request.route.mode.width,
                        request.route.mode.height,
                        request.route.mode.refresh_millihz,
                    )),
                )
                .await);
        }
        if selected.format != self.config.raw_layout.format
            || selected.modifier != self.config.raw_layout.modifier
        {
            return Err(self
                .finish_prepared_renderer(
                    renderer,
                    MediaPipelineError::new(
                        "capture output does not match the negotiated raw-video layout",
                    ),
                )
                .await);
        }
        let capture = match self.config.raw_layout.storage {
            RawVideoStorage::SystemMemory => {
                self.capture_setup
                    .create_actor(
                        self.capture_config(),
                        request,
                        Some(selected.offer),
                        cancellation.clone(),
                    )
                    .await
            }
            RawVideoStorage::DmaBuf => {
                let count = self.config.capture_pool_size;
                let budget = self.config.capture_pool_byte_limit;
                let mut allocation = tokio::task::spawn_blocking(move || {
                    allocate_capture_buffers(&capture_device, selected, count, budget)
                });
                let buffers = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        let _ = (&mut allocation).await;
                        return Err(self.finish_prepared_renderer(
                            renderer,
                            MediaPipelineError::new("renderer start was cancelled"),
                        ).await);
                    }
                    result = &mut allocation => match result {
                        Ok(Ok(buffers)) => buffers,
                        Ok(Err(error)) => {
                            return Err(self.finish_prepared_renderer(renderer, error).await);
                        }
                        Err(error) => {
                            return Err(self.finish_prepared_renderer(
                                renderer,
                                MediaPipelineError::new(format!(
                                    "join capture GPU allocation: {error}"
                                )),
                            ).await);
                        }
                    },
                };
                self.capture_setup
                    .create_actor_with_buffers(
                        self.capture_config(),
                        request,
                        selected,
                        buffers,
                        cancellation.clone(),
                    )
                    .await
            }
        };
        let (actor, layout) = match capture {
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
        let target = self.target(&renderer, &video, generation);
        self.install_started_generation(Generation {
            id: generation,
            stream: Stream::Prepared { renderer, video },
        })
        .await?;
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
        let Some(taken) = self.take_generation() else {
            return Err(MediaPipelineError::new(
                "no renderer generation is prepared",
            ));
        };
        if taken.generation.id != media_generation {
            let actual = taken.generation.id;
            self.restore_generation(taken);
            return Err(MediaPipelineError::new(format!(
                "renderer activation requested generation {media_generation}; prepared generation is {actual}"
            )));
        }
        let TakenGeneration { lease, generation } = taken;
        self.state = PipelineState::Starting { lease };
        match generation.stream {
            Stream::Active {
                renderer,
                video,
                monitors,
            } => {
                let result = match renderer.state() {
                    RendererStreamState::Running => video.activate().await.map_err(|error| {
                        MediaPipelineError::new(format!("resume capture video: {error}"))
                    }),
                    RendererStreamState::Failed(error) => Err(MediaPipelineError::new(error)),
                    state => Err(MediaPipelineError::new(format!(
                        "renderer generation has invalid active state {state:?}"
                    ))),
                };
                self.install_started_generation(Generation {
                    id: generation.id,
                    stream: Stream::Active {
                        renderer,
                        video,
                        monitors,
                    },
                })
                .await?;
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
                self.install_started_generation(Generation {
                    id: generation.id,
                    stream: Stream::Active {
                        renderer,
                        video,
                        monitors,
                    },
                })
                .await?;
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
            .generation()
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
        let generation = match &self.state {
            PipelineState::Running { generation, .. } => Some(generation.id),
            PipelineState::Stopping { generation_id, .. } => Some(*generation_id),
            _ => None,
        };
        if let Some(generation) = generation {
            self.stop_generation(generation).await?;
        }
        if matches!(
            self.state,
            PipelineState::Idle { .. }
                | PipelineState::Starting { .. }
                | PipelineState::Running { .. }
        ) {
            self.release_renderer_lease().await?;
        }
        Ok(())
    }
}

fn allocate_capture_buffers(
    device: &Device,
    description: drm_capture::Description,
    count: NonZeroU32,
    budget: NonZeroU64,
) -> Result<Vec<Buffer>, MediaPipelineError> {
    let format = capture_format(description.format)?;
    let mut total = 0u64;
    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(count.get() as usize)
        .map_err(|error| MediaPipelineError::new(format!("reserve capture pool: {error}")))?;
    for _ in 0..count.get() {
        let image = device
            .allocate_with_format(
                format,
                description.width,
                description.height,
                description.modifier,
            )
            .map_err(|error| {
                MediaPipelineError::new(format!("allocate GPU capture image: {error}"))
            })?;
        let layout = image.layout();
        total = total
            .checked_add(layout.allocation_size)
            .filter(|total| *total <= budget.get())
            .ok_or_else(|| MediaPipelineError::new("GPU capture pool exceeds its byte budget"))?;
        let pitch = u32::try_from(layout.pitch)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| MediaPipelineError::new("GPU capture pitch exceeds the interface"))?;
        let offset = u32::try_from(layout.offset)
            .map_err(|_| MediaPipelineError::new("GPU capture offset exceeds the interface"))?;
        let size = NonZeroU64::new(layout.allocation_size)
            .ok_or_else(|| MediaPipelineError::new("GPU capture allocation is empty"))?;
        let fd = image.export().map_err(|error| {
            MediaPipelineError::new(format!("export GPU capture image: {error}"))
        })?;
        buffers.push(
            Buffer::new_drm(fd, description.format, layout.modifier, pitch, offset, size).map_err(
                |error| MediaPipelineError::new(format!("describe GPU capture image: {error}")),
            )?,
        );
    }
    Ok(buffers)
}

fn capture_format(format: u32) -> Result<PackedFormat, MediaPipelineError> {
    Ok(match format {
        value if value == u32::from_le_bytes(*b"XR24") => PackedFormat::Bgra8,
        value if value == u32::from_le_bytes(*b"AR24") => PackedFormat::Bgra8,
        value if value == u32::from_le_bytes(*b"XB24") => PackedFormat::Rgba8,
        value if value == u32::from_le_bytes(*b"AB24") => PackedFormat::Rgba8,
        _ => {
            return Err(MediaPipelineError::new(
                "renderer capture offer has an unsupported pixel format",
            ))
        }
    })
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
        RendererStreamState::Running => Ok(()),
        RendererStreamState::Starting => Err(MediaPipelineError::new(
            "renderer worker is still starting after publication",
        )),
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
            RendererStreamState::Running => None,
            RendererStreamState::Starting => {
                Some("renderer worker returned to its starting state".into())
            }
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

    #[tokio::test]
    async fn abandoned_start_releases_its_endpoint_lease() {
        let (released, wait_released) = tokio::sync::oneshot::channel();
        let lease = CapabilityLease::new(async move {
            let _ = released.send(());
            Ok(())
        });
        let mut state = PipelineState::Starting { lease };

        state.release_lease().await.unwrap();

        assert!(matches!(state, PipelineState::Unavailable));
        wait_released.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_stop_wait_keeps_generation_cleanup_owned() {
        let id = NonZeroU64::new(7).unwrap();
        let (entered, wait_entered) = tokio::sync::oneshot::channel();
        let (resume, wait_resume) = tokio::sync::oneshot::channel();
        let (released, wait_released) = tokio::sync::oneshot::channel();
        let lease = CapabilityLease::new(async move {
            let _ = released.send(());
            Ok(())
        });
        let task = tokio::spawn(async move {
            let _ = entered.send(());
            wait_resume.await.unwrap();
            lease.release().await.map_err(|error| {
                MediaPipelineError::new(format!("release renderer endpoint: {error}"))
            })
        });
        let mut state = PipelineState::Stopping {
            generation_id: id,
            task,
        };

        let mut wait = Box::pin(state.finish_stopping(id));
        tokio::select! {
            _ = &mut wait => panic!("cleanup finished before its gate opened"),
            _ = wait_entered => {}
        }
        drop(wait);
        assert!(matches!(state, PipelineState::Stopping { .. }));
        assert!(state.release_lease().await.is_err());
        assert!(state
            .finish_stopping(NonZeroU64::new(8).unwrap())
            .await
            .is_err());

        resume.send(()).unwrap();
        state.finish_stopping(id).await.unwrap();
        wait_released.await.unwrap();
        assert!(matches!(state, PipelineState::Unavailable));
    }

    #[test]
    fn capture_formats_select_the_matching_renderer_output_encoding() {
        for format in [*b"XR24", *b"AR24"] {
            assert_eq!(
                capture_format(u32::from_le_bytes(format)).unwrap(),
                PackedFormat::Bgra8
            );
        }
        for format in [*b"XB24", *b"AB24"] {
            assert_eq!(
                capture_format(u32::from_le_bytes(format)).unwrap(),
                PackedFormat::Rgba8
            );
        }
        assert!(capture_format(u32::from_le_bytes(*b"NV12")).is_err());
    }

    #[test]
    fn selection_wait_requires_a_running_renderer() {
        let (_, state) = tokio::sync::watch::channel(RendererStreamState::Starting);
        assert!(require_running_renderer(&state).is_err());

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
        let (state, receive) = tokio::sync::watch::channel(RendererStreamState::Running);
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
