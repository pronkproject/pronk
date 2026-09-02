//! Private PipeWire publication for a grant-scoped CastKMS audio tap.

use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{audio_source_runtime, PipeWireRemote, MAX_IDENTITY_STRING_BYTES};

pub const DEFAULT_AUDIO_SOURCE_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSourceConfig {
    pub node_name: String,
    pub node_description: String,
    pub session_id: String,
    pub device_instance: String,
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub grant_id: NonZeroU32,
    pub media_generation: NonZeroU64,
}

impl AudioSourceConfig {
    fn validate(&self) -> Result<(), AudioSourceConfigurationError> {
        validate_string("node name", &self.node_name)?;
        validate_string("node description", &self.node_description)?;
        validate_string("session ID", &self.session_id)?;
        validate_string("device instance", &self.device_instance)?;
        Ok(())
    }
}

fn validate_string(field: &'static str, value: &str) -> Result<(), AudioSourceConfigurationError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_STRING_BYTES || value.contains('\0') {
        return Err(AudioSourceConfigurationError::InvalidString { field });
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioSourceConfigurationError {
    #[error("{field} is empty, too long, or contains NUL")]
    InvalidString { field: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioNodeIdentity {
    pub node_name: String,
    pub object_id: NonZeroU32,
    pub object_serial: NonZeroU64,
    pub media_generation: NonZeroU64,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioSourceRuntimeError {
    #[error("create or connect PipeWire object: {0}")]
    PipeWire(String),
    #[error("PipeWire core error {code}: {message}")]
    Core { code: i32, message: String },
    #[error("PipeWire stream error: {0}")]
    Stream(String),
    #[error("PipeWire audio source node disappeared")]
    NodeRemoved,
    #[error("the versioned WirePlumber private-media policy is unavailable")]
    PolicyUnavailable,
    #[error("PipeWire negotiated an unsupported audio format")]
    UnsupportedFormat,
    #[error("PipeWire supplied an invalid audio buffer: {0}")]
    InvalidPipeWireBuffer(&'static str),
    #[error("CastKMS audio tap failed: {0}")]
    AudioTap(String),
    #[error("PipeWire audio source loop panicked")]
    ThreadPanicked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSourceEvent {
    Failed(AudioSourceRuntimeError),
    Stopped,
}

#[derive(Debug, Error)]
pub enum AudioSourceError {
    #[error(transparent)]
    Configuration(#[from] AudioSourceConfigurationError),
    #[error("spawn PipeWire audio loop thread: {0}")]
    Spawn(std::io::Error),
    #[error(transparent)]
    Runtime(#[from] AudioSourceRuntimeError),
    #[error("PipeWire audio startup channel closed")]
    StartupClosed,
    #[error("PipeWire audio startup timed out after {0:?}")]
    StartupTimeout(Duration),
    #[error("join PipeWire audio loop task: {0}")]
    Join(String),
}

/// One media-generation-specific private audio source.
pub struct AudioSource {
    identity: AudioNodeIdentity,
    commands: pipewire::channel::Sender<audio_source_runtime::Command>,
    events: mpsc::Receiver<AudioSourceEvent>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AudioSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioSource")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl AudioSource {
    pub async fn start(
        config: AudioSourceConfig,
        tap: OwnedFd,
        remote: PipeWireRemote,
    ) -> Result<Self, AudioSourceError> {
        Self::start_with_timeout(config, tap, remote, DEFAULT_AUDIO_SOURCE_STARTUP_TIMEOUT).await
    }

    pub async fn start_with_timeout(
        config: AudioSourceConfig,
        tap: OwnedFd,
        remote: PipeWireRemote,
        startup_timeout: Duration,
    ) -> Result<Self, AudioSourceError> {
        config.validate()?;
        let audio_source_runtime::RuntimeHandle {
            commands,
            startup_cancel,
            events,
            startup,
            thread,
        } = audio_source_runtime::spawn(config, tap, remote).map_err(AudioSourceError::Spawn)?;
        let mut startup_cancellation = StartupCancellation::new(startup_cancel);
        let startup_result = match tokio::time::timeout(startup_timeout, startup).await {
            Ok(result) => result,
            Err(_) => {
                startup_cancellation.cancel();
                join_thread(thread).await?;
                return Err(AudioSourceError::StartupTimeout(startup_timeout));
            }
        };
        let identity = match startup_result {
            Ok(Ok(identity)) => identity,
            Ok(Err(error)) => {
                let _ = commands.send(audio_source_runtime::Command::Shutdown);
                join_thread(thread).await?;
                return Err(AudioSourceError::Runtime(error));
            }
            Err(_) => {
                let _ = commands.send(audio_source_runtime::Command::Shutdown);
                join_thread(thread).await?;
                return Err(AudioSourceError::StartupClosed);
            }
        };
        startup_cancellation.disarm();
        Ok(Self {
            identity,
            commands,
            events,
            thread: Some(thread),
        })
    }

    pub fn identity(&self) -> &AudioNodeIdentity {
        &self.identity
    }

    pub async fn next_event(&mut self) -> Option<AudioSourceEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) -> Result<(), AudioSourceError> {
        let _ = self.commands.send(audio_source_runtime::Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            join_thread(thread).await?;
        }
        Ok(())
    }
}

impl Drop for AudioSource {
    fn drop(&mut self) {
        let _ = self.commands.send(audio_source_runtime::Command::Shutdown);
    }
}

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

async fn join_thread(thread: JoinHandle<()>) -> Result<(), AudioSourceError> {
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .map_err(|error| AudioSourceError::Join(error.to_string()))?
        .map_err(|_| AudioSourceError::Join("PipeWire audio loop thread panicked".to_string()))
}

pub(crate) type AudioStartupSender =
    oneshot::Sender<Result<AudioNodeIdentity, AudioSourceRuntimeError>>;
pub(crate) type AudioStartupSlot = Arc<Mutex<Option<AudioStartupSender>>>;
