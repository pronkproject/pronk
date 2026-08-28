//! Reusable caller-owned DMA-BUF publication through one PipeWire video node.
//!
//! PipeWire runs on a dedicated foreign-loop thread. The Tokio-facing API is
//! bounded and never receives a DRM primary-node, grant, GEM, framebuffer, or
//! raw syncobj handle. The caller remains the sole capture owner and advances
//! its reuse timeline only after [`VideoSourceEvent::BufferReleased`].

mod audio_sink;
mod audio_sink_model;
mod audio_sink_runtime;
mod model;
mod policy_gate;
mod remote_provider;
mod runtime;
mod types;
mod video_source_actor;

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Semaphore};

pub use audio_sink::{
    AudioSinkConfigurationError, CastKmsAudioSinkRequest, CastKmsAudioSinkResolver,
    CastKmsAudioSinkResolverError, CastKmsAudioSinkTarget, DEFAULT_AUDIO_SINK_RESOLUTION_TIMEOUT,
};
pub use remote_provider::{
    BackendRemoteSet, ClassifiedSocketPaths, ClassifiedSocketPathsError,
    ClassifiedSocketRemoteProvider, PipeWireConnectionRole, PipeWireConsumerFd, PipeWireProducerFd,
    PipeWireRemoteProvider, RemoteProviderError, PIPEWIRE_BACKEND_SOCKET_NAME,
    PIPEWIRE_CORE_SOCKET_NAME,
};
pub use types::{
    ConfigurationError, PipeWireBufferTransport, PipeWireRemote, VideoBuffer, VideoBufferLayout,
    VideoDamage, VideoFrame, VideoNodeIdentity, VideoSourceConfig, VideoSourceEvent,
    VideoSourceRuntimeError, VideoSyncTimelines, MAX_FRAME_DIMENSION, MAX_IDENTITY_STRING_BYTES,
    MAX_VIDEO_BUFFERS, MIN_VIDEO_BUFFERS,
};
pub use video_source_actor::{
    VideoSourceActor, VideoSourceActorError, VideoSourceActorEvent, VideoSourceActorRuntimeError,
    VideoSourceGeneration, VideoSourceStopReport, DEFAULT_VIDEO_SOURCE_STARTUP_TIMEOUT,
};

use runtime::{Command, RuntimeHandle};

#[derive(Debug, Error)]
pub enum VideoSourceError {
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    #[error("spawn PipeWire loop thread: {0}")]
    Spawn(std::io::Error),
    #[error(transparent)]
    Runtime(#[from] VideoSourceRuntimeError),
    #[error("PipeWire startup channel closed")]
    StartupClosed,
    #[error("PipeWire startup timed out after {0:?}")]
    StartupTimeout(Duration),
    #[error("PipeWire command channel closed")]
    CommandClosed,
    #[error("PipeWire command reply channel closed")]
    ReplyClosed,
    #[error("join PipeWire loop task: {0}")]
    Join(String),
}

/// One mode/media-generation-specific PipeWire source.
///
/// Dropping sends a nonblocking shutdown request. Call [`shutdown`](Self::shutdown)
/// when teardown ordering matters, especially before destroying exported
/// capture buffers or their syncobjs.
pub struct VideoSource {
    identity: VideoNodeIdentity,
    commands: pipewire::channel::Sender<Command>,
    events: mpsc::Receiver<VideoSourceEvent>,
    command_permits: Arc<Semaphore>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for VideoSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoSource")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl VideoSource {
    pub async fn start(
        config: VideoSourceConfig,
        buffers: Vec<VideoBuffer>,
        remote: PipeWireRemote,
    ) -> Result<Self, VideoSourceError> {
        Self::start_inner(config, buffers, remote, None).await
    }

    /// Start a source with a bounded registry/stream identity handshake.
    ///
    /// On timeout the pre-handshake cancellation channel stops the foreign
    /// loop and the thread is joined before this method returns.
    pub async fn start_with_timeout(
        config: VideoSourceConfig,
        buffers: Vec<VideoBuffer>,
        remote: PipeWireRemote,
        startup_timeout: Duration,
    ) -> Result<Self, VideoSourceError> {
        Self::start_inner(config, buffers, remote, Some(startup_timeout)).await
    }

    async fn start_inner(
        config: VideoSourceConfig,
        buffers: Vec<VideoBuffer>,
        remote: PipeWireRemote,
        startup_timeout: Option<Duration>,
    ) -> Result<Self, VideoSourceError> {
        config.validate(&buffers)?;
        let buffer_count = buffers.len();
        let RuntimeHandle {
            commands,
            startup_cancel,
            events,
            startup,
            thread,
        } = runtime::spawn(config, buffers, remote).map_err(VideoSourceError::Spawn)?;
        let mut startup_cancellation = StartupCancellation::new(startup_cancel);
        let startup_result = match startup_timeout {
            Some(duration) => match tokio::time::timeout(duration, startup).await {
                Ok(result) => result,
                Err(_) => {
                    startup_cancellation.cancel();
                    join_thread(thread).await?;
                    return Err(VideoSourceError::StartupTimeout(duration));
                }
            },
            None => startup.await,
        };
        let identity = match startup_result {
            Ok(Ok(identity)) => identity,
            Ok(Err(error)) => {
                let _ = commands.send(Command::Shutdown);
                join_thread(thread).await?;
                return Err(VideoSourceError::Runtime(error));
            }
            Err(_) => {
                let _ = commands.send(Command::Shutdown);
                join_thread(thread).await?;
                return Err(VideoSourceError::StartupClosed);
            }
        };
        startup_cancellation.disarm();
        Ok(Self {
            identity,
            commands,
            events,
            command_permits: Arc::new(Semaphore::new(buffer_count)),
            thread: Some(thread),
        })
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        &self.identity
    }

    /// Publish one buffer already granted to this source by an availability
    /// event. The bounded command permit prevents an unbounded per-frame queue.
    pub async fn publish(&self, frame: VideoFrame) -> Result<(), VideoSourceError> {
        let permit = self
            .command_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| VideoSourceError::CommandClosed)?;
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Publish {
                frame,
                _permit: permit,
                reply,
            })
            .map_err(|_| VideoSourceError::CommandClosed)?;
        response
            .await
            .map_err(|_| VideoSourceError::ReplyClosed)??;
        Ok(())
    }

    /// Ask PipeWire to run one ordinary graph cycle.
    ///
    /// Driving sources need this when a consumer returns a shared buffer
    /// without emitting `RequestProcess`. The graph's process callback remains
    /// the only place that dequeues and reports returned buffers.
    pub(crate) async fn trigger_process(&self) -> Result<(), VideoSourceError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::TriggerProcess { reply })
            .map_err(|_| VideoSourceError::CommandClosed)?;
        response.await.map_err(|_| VideoSourceError::ReplyClosed)?;
        Ok(())
    }

    pub async fn next_event(&mut self) -> Option<VideoSourceEvent> {
        self.events.recv().await
    }

    /// Stop the PipeWire loop and join its thread before returning.
    pub async fn shutdown(mut self) -> Result<(), VideoSourceError> {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            join_thread(thread).await?;
        }
        Ok(())
    }
}

/// Cancellation guard for the part of startup that precedes normal command
/// attachment. Dropping a caller's startup future must not strand its foreign
/// PipeWire loop thread.
struct StartupCancellation {
    sender: Option<pipewire::channel::Sender<()>>,
}

impl StartupCancellation {
    fn new(sender: pipewire::channel::Sender<()>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    fn cancel(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(());
        }
    }

    fn disarm(&mut self) {
        self.sender = None;
    }
}

impl Drop for StartupCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Drop for VideoSource {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

async fn join_thread(thread: JoinHandle<()>) -> Result<(), VideoSourceError> {
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .map_err(|error| VideoSourceError::Join(error.to_string()))?
        .map_err(|_| VideoSourceError::Join("PipeWire loop thread panicked".to_string()))
}
