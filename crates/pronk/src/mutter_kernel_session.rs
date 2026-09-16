//! Translate Mutter's display broker into application-owned capabilities.

use std::io;
use std::num::NonZeroU64;
use std::sync::Arc;

use async_trait::async_trait;
use pronk_capture_broker::{Provider, Session};
use pronk_core::output::{CastKmsOutput, OutputConnection};
use tokio_util::sync::CancellationToken;

use crate::capability_lease::CapabilityLease;
use crate::kernel_session::{
    KernelSession, KernelSessionControl, KernelSessionError, MonitorCapabilities,
};
use crate::kernel_session_provider::KernelSessionProvider;
use crate::renderer_session::{
    RendererAccess, RendererMigration, RendererProvider, RendererSession, RendererSessionError,
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
        let renderer = session
            .take_renderer_access()
            .map_err(|error| KernelSessionError::failed("take renderer authority", error))?;
        Ok(Self::new(
            session.id(),
            Box::new(MutterSession(session)),
            capture,
            Some(renderer_access(renderer)),
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

fn renderer_error(error: pronk_capture_broker::RendererSessionError) -> RendererSessionError {
    match error {
        pronk_capture_broker::RendererSessionError::Cancelled => RendererSessionError::Cancelled,
        pronk_capture_broker::RendererSessionError::Timeout => RendererSessionError::Timeout,
        pronk_capture_broker::RendererSessionError::WorkerStopped => {
            RendererSessionError::Unavailable
        }
        error => RendererSessionError::failed("Mutter renderer session", error),
    }
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

#[derive(Debug)]
struct MutterRenderer(pronk_capture_broker::RendererSessionAccess);

fn renderer_access(access: pronk_capture_broker::RendererAccess) -> RendererAccess {
    let (fd, endpoint_id, render_node, session) = access.into_capability();
    let release = session.clone();
    let lease = CapabilityLease::new(async move {
        release
            .release_renderer(endpoint_id)
            .await
            .map_err(io::Error::other)
    });
    let issuer = Arc::new(MutterRenderer(session));
    RendererAccess::new(
        fd,
        lease,
        render_node,
        RendererSession::new(issuer.clone(), Some(issuer)),
    )
}

#[async_trait]
impl RendererProvider for MutterRenderer {
    async fn acquire(
        &self,
        cancellation: CancellationToken,
    ) -> Result<RendererAccess, RendererSessionError> {
        self.0
            .acquire_renderer(cancellation)
            .await
            .map(renderer_access)
            .map_err(renderer_error)
    }
}

#[async_trait]
impl RendererMigration for MutterRenderer {
    async fn install_transition(
        &self,
        transition: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), RendererSessionError> {
        self.0
            .install_transition(transition, cancellation)
            .await
            .map_err(renderer_error)
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
