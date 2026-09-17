//! One authorized display lifetime, independent of the capability issuer.

use std::io;
use std::num::NonZeroU64;

use async_trait::async_trait;
use drm_capture::Access as CaptureAccess;

use crate::renderer_session::RendererAccess;

#[derive(Debug, thiserror::Error)]
pub enum KernelSessionError {
    #[error("kernel display authorization was cancelled")]
    Cancelled,
    #[error("kernel display operation timed out")]
    Timeout,
    #[error("kernel display issuer is unavailable")]
    Unavailable,
    #[error("CastKMS output has an invalid zero {0}")]
    InvalidOutput(&'static str),
    #[error("the selected kernel display issuer does not provide audio")]
    UnsupportedAudio,
    #[error("{operation}: {message}")]
    Failed {
        operation: &'static str,
        message: String,
    },
}

impl KernelSessionError {
    pub fn failed(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Failed {
            operation,
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorCapabilities {
    pub max_edid_size: usize,
}

/// Monitor control and the obligation to release its authorization lifetime.
///
/// Implementations retain whatever keeps the issuer's authority valid. Drop
/// must request release, including if an explicit release future is abandoned.
/// Synchronous monitor operations may block and belong on a blocking worker.
#[async_trait]
pub trait KernelSessionControl: std::fmt::Debug + Send + Sync + 'static {
    fn monitor_capabilities(&self) -> io::Result<MonitorCapabilities>;
    fn attach_monitor(&self, edid: Option<&[u8]>) -> io::Result<()>;
    fn detach_monitor(&self) -> io::Result<()>;
    async fn release(self: Box<Self>) -> Result<(), KernelSessionError>;
}

/// Separate capture and renderer capabilities under one display lifetime.
///
/// Issuer identities, connections and revocation authority stay inside control.
/// Taking a media capability does not transfer the display lifetime itself.
#[derive(Debug)]
pub struct KernelSession {
    capture: CaptureAccess,
    renderer: Option<RendererAccess>,
    control: Box<dyn KernelSessionControl>,
    id: NonZeroU64,
}

impl KernelSession {
    /// Bind capabilities to a diagnostic identity local to their provider.
    ///
    /// The identity is not authority and must not be compared across providers.
    pub fn new(
        id: NonZeroU64,
        control: Box<dyn KernelSessionControl>,
        capture: CaptureAccess,
        renderer: Option<RendererAccess>,
    ) -> Self {
        Self {
            capture,
            renderer,
            control,
            id,
        }
    }

    pub fn id(&self) -> NonZeroU64 {
        self.id
    }

    pub fn capture_access(&self) -> io::Result<CaptureAccess> {
        self.capture.try_clone()
    }

    pub fn take_renderer_access(&mut self) -> io::Result<RendererAccess> {
        self.renderer.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the display session has no renderer capability",
            )
        })
    }

    pub fn monitor_capabilities(&self) -> io::Result<MonitorCapabilities> {
        self.control.monitor_capabilities()
    }

    pub fn attach_monitor(&self, edid: Option<&[u8]>) -> io::Result<()> {
        self.control.attach_monitor(edid)
    }

    pub fn detach_monitor(&self) -> io::Result<()> {
        self.control.detach_monitor()
    }

    pub async fn release(self) -> Result<(), KernelSessionError> {
        let Self {
            capture,
            renderer,
            control,
            ..
        } = self;
        drop(capture);
        let renderer = match renderer {
            Some(renderer) => renderer.release().await,
            None => Ok(()),
        };
        let session = control.release().await;
        match (renderer, session) {
            (Ok(()), result) => result,
            (Err(error), Ok(())) => {
                Err(KernelSessionError::failed("release unused renderer", error))
            }
            (Err(renderer), Err(session)) => Err(KernelSessionError::failed(
                "release display capabilities",
                format!("renderer: {renderer}; session: {session}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_lease::CapabilityLease;
    use crate::renderer_session::{RendererProvider, RendererSession, RendererSessionError};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct Control {
        events: Arc<Mutex<Vec<&'static str>>>,
        lease: CapabilityLease,
    }

    #[async_trait]
    impl KernelSessionControl for Control {
        fn monitor_capabilities(&self) -> io::Result<MonitorCapabilities> {
            Ok(MonitorCapabilities { max_edid_size: 512 })
        }

        fn attach_monitor(&self, _: Option<&[u8]>) -> io::Result<()> {
            self.events.lock().unwrap().push("attach");
            Ok(())
        }

        fn detach_monitor(&self) -> io::Result<()> {
            self.events.lock().unwrap().push("detach");
            Ok(())
        }

        async fn release(self: Box<Self>) -> Result<(), KernelSessionError> {
            self.lease
                .release()
                .await
                .map_err(|error| KernelSessionError::failed("release test session", error))
        }
    }

    fn session() -> (KernelSession, Arc<Mutex<Vec<&'static str>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::clone(&events);
        let control = Control {
            events: Arc::clone(&events),
            lease: CapabilityLease::new(async move {
                released.lock().unwrap().push("release");
                Ok(())
            }),
        };
        (
            KernelSession::new(
                NonZeroU64::new(1).unwrap(),
                Box::new(control),
                CaptureAccess::from_fd(std::fs::File::open("/dev/null").unwrap().into()),
                None,
            ),
            events,
        )
    }

    #[tokio::test]
    async fn display_control_needs_no_broker_or_renderer() {
        let (mut session, events) = session();
        assert_eq!(session.id().get(), 1);
        assert_eq!(session.monitor_capabilities().unwrap().max_edid_size, 512);
        assert_eq!(
            session.take_renderer_access().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        session.attach_monitor(None).unwrap();
        session.detach_monitor().unwrap();
        session.release().await.unwrap();
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &["attach", "detach", "release"]
        );
    }

    #[tokio::test]
    async fn capture_access_does_not_own_the_display_lifetime() {
        let (session, events) = session();
        let capture = session.capture_access().unwrap();
        drop(capture);
        tokio::task::yield_now().await;
        assert!(events.lock().unwrap().is_empty());
        session.release().await.unwrap();
        assert_eq!(events.lock().unwrap().as_slice(), &["release"]);
    }

    #[tokio::test]
    async fn retained_capture_does_not_prevent_display_release() {
        let (session, events) = session();
        let capture = session.capture_access().unwrap();
        session.release().await.unwrap();
        assert_eq!(events.lock().unwrap().as_slice(), &["release"]);
        drop(capture);
    }

    #[tokio::test]
    async fn abandoned_sessions_still_release_authority() {
        let (session, events) = session();
        drop(session);
        tokio::task::yield_now().await;
        assert_eq!(events.lock().unwrap().as_slice(), &["release"]);
    }

    #[derive(Debug)]
    struct NoReplacement;

    #[async_trait]
    impl RendererProvider for NoReplacement {
        async fn acquire(
            &self,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<RendererAccess, RendererSessionError> {
            Err(RendererSessionError::Unavailable)
        }
    }

    fn renderer(events: Arc<Mutex<Vec<&'static str>>>, fail: bool) -> RendererAccess {
        RendererAccess::new(
            std::fs::File::open("/dev/null").unwrap().into(),
            CapabilityLease::new(async move {
                events.lock().unwrap().push("renderer");
                if fail {
                    Err(io::Error::other("renderer release failed"))
                } else {
                    Ok(())
                }
            }),
            "/dev/dri/renderD128".into(),
            RendererSession::new(Arc::new(NoReplacement)),
        )
    }

    #[tokio::test]
    async fn unused_renderer_is_released_before_its_display_session() {
        let (mut session, events) = session();
        session.renderer = Some(renderer(Arc::clone(&events), false));
        session.release().await.unwrap();
        assert_eq!(events.lock().unwrap().as_slice(), &["renderer", "release"]);
    }

    #[tokio::test]
    async fn renderer_cleanup_failure_does_not_skip_display_release() {
        let (mut session, events) = session();
        session.renderer = Some(renderer(Arc::clone(&events), true));
        assert!(session
            .release()
            .await
            .unwrap_err()
            .to_string()
            .contains("renderer release failed"));
        assert_eq!(events.lock().unwrap().as_slice(), &["renderer", "release"]);
    }

    #[tokio::test]
    async fn renderer_handoff_moves_only_its_own_cleanup_obligation() {
        let (mut session, events) = session();
        session.renderer = Some(renderer(Arc::clone(&events), false));
        let access = session.take_renderer_access().unwrap();
        assert!(session.take_renderer_access().is_err());
        tokio::task::yield_now().await;
        assert!(events.lock().unwrap().is_empty());
        access.release().await.unwrap();
        assert_eq!(events.lock().unwrap().as_slice(), &["renderer"]);
        session.release().await.unwrap();
        assert_eq!(events.lock().unwrap().as_slice(), &["renderer", "release"]);
    }
}
