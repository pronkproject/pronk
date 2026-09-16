//! Application boundary for acquiring one authorized kernel display lifetime.

use async_trait::async_trait;
use pronk_capture_broker::{Provider as BrokerProvider, Session as BrokerSession};
use pronk_core::output::{CastKmsOutput, OutputConnection};
use tokio_util::sync::CancellationToken;

pub type KernelSession = BrokerSession;

#[derive(Debug, thiserror::Error)]
pub enum KernelSessionError {
    #[error("kernel display authorization was cancelled")]
    Cancelled,
    #[error("acquire kernel display session: {0}")]
    Broker(#[source] pronk_capture_broker::Error),
    #[error("CastKMS output has an invalid zero {0}")]
    InvalidOutput(&'static str),
    #[error("brokered kernel display sessions do not yet provide audio")]
    UnsupportedAudio,
}

#[async_trait]
pub trait KernelSessionProvider: std::fmt::Debug + Send + Sync + 'static {
    /// Return whether acquiring the output is worth attempting.
    ///
    /// The result is only a preliminary selection rule. The provider must
    /// still arbitrate ownership when `acquire` runs.
    fn may_acquire(&self, output: &CastKmsOutput) -> bool {
        output.is_available()
    }

    async fn acquire(
        &self,
        output: &CastKmsOutput,
        audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError>;
}

#[async_trait]
impl KernelSessionProvider for BrokerProvider {
    fn may_acquire(&self, output: &CastKmsOutput) -> bool {
        broker_may_acquire(output)
    }

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
        .map_err(|error| match error {
            pronk_capture_broker::Error::Cancelled => KernelSessionError::Cancelled,
            error => KernelSessionError::Broker(error),
        })
    }
}

fn broker_may_acquire(output: &CastKmsOutput) -> bool {
    matches!(
        output.connection,
        OutputConnection::Connected | OutputConnection::Disconnected
    )
}

fn validate_brokered_features(audio_enabled: bool) -> Result<(), KernelSessionError> {
    if audio_enabled {
        Err(KernelSessionError::UnsupportedAudio)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pronk_core::output::{CastKmsOutputId, OutputConnection};

    use super::*;

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

    #[test]
    fn brokered_sessions_reject_audio_requests() {
        assert!(matches!(
            validate_brokered_features(true),
            Err(KernelSessionError::UnsupportedAudio)
        ));
        assert!(validate_brokered_features(false).is_ok());
    }

    #[test]
    fn broker_attempts_ownership_for_connected_outputs() {
        let mut candidate = output();
        candidate.connection = OutputConnection::Connected;
        assert!(broker_may_acquire(&candidate));

        candidate.connection = OutputConnection::Unknown;
        assert!(!broker_may_acquire(&candidate));
    }
}
