//! Bounded connector-bound CastKMS audio sink lookup.

use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;

use crate::audio_sink_model::is_normal_absolute_path;
use crate::audio_sink_runtime;
use crate::PipeWireRemote;

pub const DEFAULT_AUDIO_SINK_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SYSFS_PATH_BYTES: usize = 4_096;
const MAX_CASTKMS_OUTPUT_INDEX: u32 = 127;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastKmsAudioSinkRequest {
    pub device_path: PathBuf,
    pub output_index: u32,
}

impl CastKmsAudioSinkRequest {
    fn validate(&self) -> Result<(), AudioSinkConfigurationError> {
        let Some(path) = self.device_path.to_str() else {
            return Err(AudioSinkConfigurationError::DevicePath);
        };
        if path.len() > MAX_SYSFS_PATH_BYTES || !is_normal_absolute_path(&self.device_path) {
            return Err(AudioSinkConfigurationError::DevicePath);
        }
        if self.output_index > MAX_CASTKMS_OUTPUT_INDEX {
            return Err(AudioSinkConfigurationError::OutputIndex(self.output_index));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastKmsAudioSinkTarget {
    pub node_name: String,
    pub object_id: NonZeroU32,
    pub object_serial: NonZeroU64,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioSinkConfigurationError {
    #[error("CastKMS device path is not a bounded, normalized absolute UTF-8 path")]
    DevicePath,
    #[error("CastKMS output index {0} exceeds the supported range")]
    OutputIndex(u32),
}

#[derive(Debug, Error)]
pub enum CastKmsAudioSinkResolverError {
    #[error(transparent)]
    Configuration(#[from] AudioSinkConfigurationError),
    #[error("spawn PipeWire audio resolver thread: {0}")]
    Spawn(std::io::Error),
    #[error("resolve CastKMS audio sink: {0}")]
    Runtime(String),
    #[error("PipeWire audio resolver result channel closed")]
    ResultClosed,
    #[error("PipeWire audio resolution timed out after {0:?}")]
    Timeout(Duration),
    #[error("join PipeWire audio resolver thread: {0}")]
    Join(String),
}

#[derive(Debug, Clone)]
pub struct CastKmsAudioSinkResolver {
    timeout: Duration,
}

impl Default for CastKmsAudioSinkResolver {
    fn default() -> Self {
        Self::new(DEFAULT_AUDIO_SINK_RESOLUTION_TIMEOUT)
    }
}

impl CastKmsAudioSinkResolver {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    pub async fn resolve(
        &self,
        request: CastKmsAudioSinkRequest,
        remote: PipeWireRemote,
    ) -> Result<CastKmsAudioSinkTarget, CastKmsAudioSinkResolverError> {
        request.validate()?;
        let audio_sink_runtime::RuntimeHandle {
            cancel,
            result,
            thread,
        } = audio_sink_runtime::spawn(request, remote)
            .map_err(CastKmsAudioSinkResolverError::Spawn)?;
        let mut cancellation = ResolutionCancellation::new(cancel);
        let resolved = match tokio::time::timeout(self.timeout, result).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                cancellation.cancel();
                join_thread(thread).await?;
                return Err(CastKmsAudioSinkResolverError::ResultClosed);
            }
            Err(_) => {
                cancellation.cancel();
                join_thread(thread).await?;
                return Err(CastKmsAudioSinkResolverError::Timeout(self.timeout));
            }
        };
        cancellation.disarm();
        join_thread(thread).await?;
        resolved.map_err(|error| CastKmsAudioSinkResolverError::Runtime(error.to_string()))
    }
}

struct ResolutionCancellation {
    sender: Option<pipewire::channel::Sender<()>>,
}

impl ResolutionCancellation {
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

impl Drop for ResolutionCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn join_thread(thread: JoinHandle<()>) -> Result<(), CastKmsAudioSinkResolverError> {
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .map_err(|error| CastKmsAudioSinkResolverError::Join(error.to_string()))?
        .map_err(|_| {
            CastKmsAudioSinkResolverError::Join("PipeWire audio resolver thread panicked".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rejects_noncanonical_paths_and_unbounded_indexes() {
        for path in [
            PathBuf::from("sys/devices/faux/castkms"),
            PathBuf::from("/sys/devices/faux/../faux/castkms"),
        ] {
            assert_eq!(
                CastKmsAudioSinkRequest {
                    device_path: path,
                    output_index: 0,
                }
                .validate(),
                Err(AudioSinkConfigurationError::DevicePath)
            );
        }
        assert_eq!(
            CastKmsAudioSinkRequest {
                device_path: "/sys/devices/faux/castkms".into(),
                output_index: 128,
            }
            .validate(),
            Err(AudioSinkConfigurationError::OutputIndex(128))
        );
    }
}
