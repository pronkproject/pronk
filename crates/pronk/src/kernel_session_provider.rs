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
            Self::Legacy(lease) => formatter.debug_tuple("Legacy").field(lease).finish(),
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
        _audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError> {
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
