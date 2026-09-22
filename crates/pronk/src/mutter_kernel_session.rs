//! Translate Mutter's display broker into application-owned capabilities.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use pronk_capture_broker::{Provider, RendererEndpoint, RendererIssuer, Session};
use pronk_core::output::{CastKmsOutput, OutputConnection};
use tokio_util::sync::CancellationToken;

use crate::capability_lease::CapabilityLease;
use crate::kernel_session::{
    KernelSession, KernelSessionControl, KernelSessionError, MonitorCapabilities,
};
use crate::kernel_session_provider::KernelSessionProvider;
use crate::renderer_session::{
    RendererAccess, RendererProvider, RendererSession, RendererSessionError,
};

#[async_trait]
impl KernelSessionProvider for Provider {
    fn may_acquire(&self, output: &CastKmsOutput) -> bool {
        broker_may_acquire(output)
    }

    async fn acquire(
        &self,
        output: &CastKmsOutput,
        audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError> {
        validate_features(audio_enabled)?;
        let crtc_id = std::num::NonZeroU32::new(output.crtc_id)
            .ok_or(KernelSessionError::InvalidOutput("CRTC ID"))?;
        let connector_id = std::num::NonZeroU32::new(output.connector_id)
            .ok_or(KernelSessionError::InvalidOutput("connector ID"))?;
        let session = Provider::acquire(
            self,
            pronk_capture_broker::Target {
                device_major: output.device_major,
                device_minor: output.device_minor,
                crtc_id,
                connector_id,
            },
            cancellation.clone(),
        )
        .await
        .map_err(session_error)?;
        if cancellation.is_cancelled() {
            return Err(KernelSessionError::Cancelled);
        }
        session.try_into()
    }
}

impl TryFrom<Session> for KernelSession {
    type Error = KernelSessionError;

    fn try_from(mut session: Session) -> Result<Self, Self::Error> {
        let capture = session.capture_access().map_err(|error| {
            KernelSessionError::failed("retain final-image capture authority", error)
        })?;
        let renderer = renderer_access(session.take_renderer().map_err(|error| {
            KernelSessionError::failed("retain Mutter renderer authority", error)
        })?);
        Ok(Self::new(
            session.id(),
            Box::new(MutterSession(session)),
            capture,
            Some(renderer),
        ))
    }
}

fn broker_may_acquire(output: &CastKmsOutput) -> bool {
    matches!(
        output.connection,
        OutputConnection::Connected | OutputConnection::Disconnected
    )
}

fn validate_features(audio_enabled: bool) -> Result<(), KernelSessionError> {
    if audio_enabled {
        Err(KernelSessionError::UnsupportedAudio)
    } else {
        Ok(())
    }
}

fn session_error(error: pronk_capture_broker::Error) -> KernelSessionError {
    match error {
        pronk_capture_broker::Error::Cancelled => KernelSessionError::Cancelled,
        pronk_capture_broker::Error::Timeout => KernelSessionError::Timeout,
        pronk_capture_broker::Error::WorkerStopped => KernelSessionError::Unavailable,
        error => KernelSessionError::failed("Mutter display session", error),
    }
}

fn renderer_error(error: pronk_capture_broker::Error) -> RendererSessionError {
    match error {
        pronk_capture_broker::Error::Cancelled => RendererSessionError::Cancelled,
        pronk_capture_broker::Error::Timeout => RendererSessionError::Timeout,
        pronk_capture_broker::Error::WorkerStopped => RendererSessionError::Unavailable,
        error => RendererSessionError::failed("Mutter renderer", error),
    }
}

/// Mutter is the trusted issuer for the display session and binds every
/// replacement endpoint to the same private broker session.
#[derive(Debug, Clone)]
struct MutterRendererProvider(RendererIssuer);

#[async_trait]
impl RendererProvider for MutterRendererProvider {
    async fn acquire(
        &self,
        cancellation: CancellationToken,
    ) -> Result<RendererAccess, RendererSessionError> {
        if cancellation.is_cancelled() {
            return Err(RendererSessionError::Cancelled);
        }
        self.0
            .acquire()
            .await
            .map(renderer_access)
            .map_err(renderer_error)
    }
}

fn renderer_access(endpoint: RendererEndpoint) -> RendererAccess {
    let (fd, id, issuer) = endpoint.into_parts();
    let render_node = issuer.render_node().to_path_buf();
    let session = RendererSession::new(Arc::new(MutterRendererProvider(issuer.clone())));
    let lease = CapabilityLease::new(async move {
        issuer
            .release(id)
            .await
            .map_err(|error| io::Error::other(error.to_string()))
    });
    RendererAccess::new(fd, lease, render_node, session)
}

#[derive(Debug)]
struct MutterSession(Session);

#[async_trait]
impl KernelSessionControl for MutterSession {
    fn monitor_capabilities(&self) -> io::Result<MonitorCapabilities> {
        self.0
            .monitor_capabilities()
            .map(|capabilities| MonitorCapabilities {
                max_edid_size: capabilities.max_edid_size,
                cec_transport: capabilities.cec_transport,
            })
    }

    fn attach_monitor(&self, edid: Option<&[u8]>) -> io::Result<()> {
        self.0.attach_monitor(edid)
    }

    fn detach_monitor(&self) -> io::Result<()> {
        self.0.detach_monitor()
    }

    async fn release(self: Box<Self>) -> Result<(), KernelSessionError> {
        self.0.release().await.map_err(session_error)
    }
}

#[cfg(test)]
#[path = "mutter_kernel_session/lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use pronk_core::output::CastKmsOutputId;
    use std::path::PathBuf;

    pub(super) fn output() -> CastKmsOutput {
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
            validate_features(true),
            Err(KernelSessionError::UnsupportedAudio)
        ));
        assert!(validate_features(false).is_ok());
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
