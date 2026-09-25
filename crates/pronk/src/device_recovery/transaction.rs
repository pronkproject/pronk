//! Owned phases of one Device-session replacement attempt.

use pronk_dbus::DeviceInfo;
use std::num::NonZeroU64;
use tokio_util::sync::CancellationToken;

use super::{
    DeviceSessionFactoryError, DeviceSessionFactoryPort, DeviceSessionRecoveryEvent,
    PreparedDeviceSession,
};
use crate::device_session_port::{DeviceSessionEventPort, DeviceSessionStopReason};
use crate::preparation::PreparedCastDevice;
use crate::replaceable_device_session::{
    DeviceSessionInstallationPermit, DeviceSessionReplacement, DeviceSessionReplacementHandle,
};

pub(super) enum AttemptError {
    Cancelled,
    Failed(String),
}

pub(super) struct RecoveryAttempt {
    request_generation: u64,
    device: DeviceInfo,
    session_generation: NonZeroU64,
    cancellation: CancellationToken,
}

struct RetiredAttempt<'a> {
    attempt: RecoveryAttempt,
    permit: DeviceSessionInstallationPermit<'a>,
    retired_session_cleanup_error: Option<String>,
}

struct PreparedAttempt<'a> {
    attempt: RecoveryAttempt,
    permit: DeviceSessionInstallationPermit<'a>,
    retired_session_cleanup_error: Option<String>,
    recovered: PreparedDeviceSession,
}

pub(super) struct ReadySession {
    pub(super) session_generation: NonZeroU64,
    pub(super) events: Box<dyn DeviceSessionEventPort>,
    pub(super) event: DeviceSessionRecoveryEvent,
}

impl PreparedDeviceSession {
    async fn cleanup(self) -> Option<String> {
        self.events.shutdown().await;
        self.session
            .stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .err()
            .map(|error| error.to_string())
    }
}

impl RecoveryAttempt {
    pub(super) fn reserve(
        request_generation: u64,
        device: DeviceInfo,
        cancellation: CancellationToken,
        last_session_generation: &mut NonZeroU64,
    ) -> Option<Self> {
        let session_generation = last_session_generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)?;
        // Ambiguous creation attempts consume their generation too.
        *last_session_generation = session_generation;
        Some(Self {
            request_generation,
            device,
            session_generation,
            cancellation,
        })
    }

    pub(super) async fn execute(
        self,
        replacement: &mut DeviceSessionReplacementHandle,
        factory: &mut dyn DeviceSessionFactoryPort,
        expected: &PreparedCastDevice,
    ) -> Result<ReadySession, AttemptError> {
        self.retire(replacement)
            .await?
            .prepare(factory, expected)
            .await?
            .install()
            .await
    }

    async fn retire<'a>(
        self,
        replacement: &'a mut DeviceSessionReplacementHandle,
    ) -> Result<RetiredAttempt<'a>, AttemptError> {
        if self.cancellation.is_cancelled() {
            return Err(AttemptError::Cancelled);
        }
        let permit = replacement.retire_current().await.map_err(|error| {
            AttemptError::Failed(format!("retire current Device session: {error}"))
        })?;
        let retired_session_cleanup_error = permit.retirement().cleanup_error.clone();
        Ok(RetiredAttempt {
            attempt: self,
            permit,
            retired_session_cleanup_error,
        })
    }
}

impl<'a> RetiredAttempt<'a> {
    async fn prepare(
        self,
        factory: &mut dyn DeviceSessionFactoryPort,
        expected: &PreparedCastDevice,
    ) -> Result<PreparedAttempt<'a>, AttemptError> {
        let recovered = factory
            .create_prepared_session(
                self.attempt.device.clone(),
                self.attempt.session_generation,
                self.attempt.cancellation.clone(),
            )
            .await
            .map_err(|error| match error {
                DeviceSessionFactoryError::Cancelled => AttemptError::Cancelled,
                other => AttemptError::Failed(other.to_string()),
            })?;
        if self.attempt.cancellation.is_cancelled() {
            let _ = recovered.cleanup().await;
            return Err(AttemptError::Cancelled);
        }
        if let Err(error) = expected.validate_recovery(&recovered.prepared) {
            let cleanup = recovered.cleanup().await;
            let diagnostic = match cleanup {
                Some(cleanup) => format!("recovered Device session is incompatible: {error}; replacement cleanup also failed: {cleanup}"),
                None => format!("recovered Device session is incompatible: {error}"),
            };
            return Err(AttemptError::Failed(diagnostic));
        }
        Ok(PreparedAttempt {
            attempt: self.attempt,
            permit: self.permit,
            retired_session_cleanup_error: self.retired_session_cleanup_error,
            recovered,
        })
    }
}

impl PreparedAttempt<'_> {
    async fn install(self) -> Result<ReadySession, AttemptError> {
        let PreparedDeviceSession {
            session, events, ..
        } = self.recovered;
        match self
            .permit
            .install(DeviceSessionReplacement {
                session_generation: self.attempt.session_generation,
                session,
            })
            .await
        {
            Ok(report) => Ok(ReadySession {
                session_generation: report.installed_session_generation,
                events,
                event: DeviceSessionRecoveryEvent::Ready {
                    request_generation: self.attempt.request_generation,
                    device: self.attempt.device,
                    session_generation: report.installed_session_generation,
                    retired_session_cleanup_error: self.retired_session_cleanup_error,
                },
            }),
            Err(error) => {
                events.shutdown().await;
                Err(AttemptError::Failed(format!(
                    "install recovered Device session: {error}"
                )))
            }
        }
    }
}
