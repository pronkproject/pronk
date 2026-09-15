//! Application boundary for acquiring one authorized kernel display lifetime.

use std::sync::Arc;

use async_trait::async_trait;
use pronk_capture_broker::{Provider as BrokerProvider, Session as BrokerSession};
use pronk_core::grant::{
    GrantAcquisitionError, GrantLease, GrantProfile, GrantProvider, GrantTarget,
};
use pronk_core::output::CastKmsOutput;
use tokio_util::sync::CancellationToken;

pub enum KernelSession {
    Brokered(BrokerSession),
    Legacy(GrantLease),
}

impl std::fmt::Debug for KernelSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Brokered(session) => formatter
                .debug_tuple("Brokered")
                .field(&session.id())
                .finish(),
            Self::Legacy(_) => formatter.debug_tuple("Legacy").finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KernelSessionError {
    #[error("kernel display authorization was cancelled")]
    Cancelled,
    #[error("acquire brokered kernel display session: {0}")]
    Broker(#[source] pronk_capture_broker::Error),
    #[error("acquire legacy CastKMS grant: {0}")]
    Legacy(#[source] GrantAcquisitionError),
    #[error("CastKMS output has an invalid zero {0}")]
    InvalidOutput(&'static str),
    #[error("brokered kernel display sessions do not yet provide audio")]
    UnsupportedAudio,
}

#[async_trait]
pub trait KernelSessionProvider: std::fmt::Debug + Send + Sync + 'static {
    async fn acquire(
        &self,
        output: &CastKmsOutput,
        audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError>;
}

#[async_trait]
impl KernelSessionProvider for BrokerProvider {
    async fn acquire(
        &self,
        output: &CastKmsOutput,
        audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError> {
        validate_brokered_features(audio_enabled)?;
        let crtc_id = std::num::NonZeroU32::new(output.crtc_id)
            .ok_or(KernelSessionError::InvalidOutput("CRTC ID"))?;
        let connector_id = std::num::NonZeroU32::new(output.connector_id)
            .ok_or(KernelSessionError::InvalidOutput("connector ID"))?;
        BrokerProvider::acquire(
            self,
            pronk_capture_broker::Target {
                device_major: output.device_major,
                device_minor: output.device_minor,
                crtc_id,
                connector_id,
            },
            cancellation,
        )
        .await
        .map(KernelSession::Brokered)
        .map_err(|error| match error {
            pronk_capture_broker::Error::Cancelled => KernelSessionError::Cancelled,
            error => KernelSessionError::Broker(error),
        })
    }
}

fn validate_brokered_features(audio_enabled: bool) -> Result<(), KernelSessionError> {
    if audio_enabled {
        Err(KernelSessionError::UnsupportedAudio)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct LegacyKernelSessionProvider {
    grants: Arc<dyn GrantProvider>,
}

impl LegacyKernelSessionProvider {
    pub fn new(grants: Arc<dyn GrantProvider>) -> Self {
        Self { grants }
    }
}

#[async_trait]
impl KernelSessionProvider for LegacyKernelSessionProvider {
    async fn acquire(
        &self,
        output: &CastKmsOutput,
        audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError> {
        let profile = if audio_enabled {
            GrantProfile::DisplayCecAudioV1
        } else {
            GrantProfile::DisplayCecV1
        };
        self.grants
            .acquire(
                GrantTarget {
                    device_major: output.device_major,
                    device_minor: output.device_minor,
                    connector_id: output.connector_id,
                    profile,
                },
                cancellation,
            )
            .await
            .map(KernelSession::Legacy)
            .map_err(|error| match error {
                GrantAcquisitionError::Cancelled => KernelSessionError::Cancelled,
                error => KernelSessionError::Legacy(error),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use pronk_core::output::{CastKmsOutputId, OutputConnection};

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingGrantProvider {
        target: Mutex<Option<GrantTarget>>,
    }

    #[async_trait]
    impl GrantProvider for RecordingGrantProvider {
        async fn acquire(
            &self,
            target: GrantTarget,
            _cancellation: CancellationToken,
        ) -> Result<GrantLease, GrantAcquisitionError> {
            *self.target.lock().unwrap() = Some(target);
            Err(GrantAcquisitionError::Cancelled)
        }
    }

    fn output() -> CastKmsOutput {
        CastKmsOutput {
            id: CastKmsOutputId {
                device_path: PathBuf::from("/sys/devices/virtual/castkms"),
                output_index: 3,
            },
            node_path: PathBuf::from("/dev/dri/card9"),
            device_major: 226,
            device_minor: 9,
            crtc_id: 17,
            connector_id: 29,
            connector_name: "Virtual-4".into(),
            connection: OutputConnection::Disconnected,
        }
    }

    #[tokio::test]
    async fn legacy_adapter_selects_the_complete_audio_profile() {
        let grants = Arc::new(RecordingGrantProvider::default());
        let provider = LegacyKernelSessionProvider::new(grants.clone());
        assert!(matches!(
            provider
                .acquire(&output(), true, CancellationToken::new())
                .await,
            Err(KernelSessionError::Cancelled)
        ));
        let target = grants.target.lock().unwrap().clone().unwrap();
        assert_eq!(target.device_major, 226);
        assert_eq!(target.device_minor, 9);
        assert_eq!(target.connector_id, 29);
        assert_eq!(target.profile, GrantProfile::DisplayCecAudioV1);
    }
}
