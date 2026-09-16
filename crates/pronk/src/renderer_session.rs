//! Application-owned renderer authority and optional compositor cooperation.

use std::io;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use castkms_renderer::Renderer;
use tokio_util::sync::CancellationToken;

use crate::capability_lease::CapabilityLease;

#[derive(Debug, thiserror::Error)]
pub enum RendererSessionError {
    #[error("renderer operation was cancelled")]
    Cancelled,
    #[error("renderer operation timed out")]
    Timeout,
    #[error("renderer issuer is unavailable")]
    Unavailable,
    #[error("the renderer session does not provide compositor-assisted migration")]
    MigrationUnavailable,
    #[error("{operation}: {message}")]
    Failed {
        operation: &'static str,
        message: String,
    },
}

impl RendererSessionError {
    pub fn failed(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Failed {
            operation,
            message: error.to_string(),
        }
    }
}

/// Issuer-specific acquisition of a renderer endpoint for one display lifetime.
///
/// Acquiring authority does not migrate the display or select a renderer. A
/// cancelled or dropped acquisition must still return any late-issued endpoint
/// to its issuer. Implementations bound outstanding acquisition and cleanup.
#[async_trait]
pub trait RendererProvider: std::fmt::Debug + Send + Sync + 'static {
    async fn acquire(
        &self,
        cancellation: CancellationToken,
    ) -> Result<RendererAccess, RendererSessionError>;
}

/// Optional cooperation with the compositor for a registered kernel transition.
///
/// An error does not promise that a request delivered to the compositor was
/// cancelled. The caller must retire the candidate if installation fails.
#[async_trait]
pub trait RendererMigration: std::fmt::Debug + Send + Sync + 'static {
    async fn install_transition(
        &self,
        transition: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), RendererSessionError>;
}

/// Independent routes to obtain authority and request compositor cooperation.
#[derive(Debug, Clone)]
pub struct RendererSession {
    provider: Arc<dyn RendererProvider>,
    migration: Option<Arc<dyn RendererMigration>>,
}

impl RendererSession {
    pub fn new(
        provider: Arc<dyn RendererProvider>,
        migration: Option<Arc<dyn RendererMigration>>,
    ) -> Self {
        Self {
            provider,
            migration,
        }
    }

    pub async fn acquire(
        &self,
        cancellation: CancellationToken,
    ) -> Result<RendererAccess, RendererSessionError> {
        let access = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RendererSessionError::Cancelled),
            result = self.provider.acquire(cancellation.clone()) => result,
        }?;
        if cancellation.is_cancelled() {
            return Err(RendererSessionError::Cancelled);
        }
        Ok(access)
    }

    /// Install the transition required by the selected renderer protocol.
    ///
    /// Missing cooperation is an error, never permission to skip a kernel
    /// activation requirement. Authority may exist independently of migration.
    pub async fn install_transition(
        &self,
        transition: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), RendererSessionError> {
        if cancellation.is_cancelled() {
            return Err(RendererSessionError::Cancelled);
        }
        let migration = self
            .migration
            .as_ref()
            .ok_or(RendererSessionError::MigrationUnavailable)?;
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RendererSessionError::Cancelled),
            result = migration.install_transition(transition, cancellation.clone()) => result,
        }
    }
}

/// One renderer descriptor and its obligation to release issuer resources.
///
/// Monitor control, final-image capture, issuer identities and revocation files
/// are not part of the capability passed to the renderer worker.
#[derive(Debug)]
pub struct RendererAccess {
    fd: OwnedFd,
    lease: CapabilityLease,
    render_node: PathBuf,
    session: RendererSession,
}

#[derive(Debug)]
pub struct OpenRenderer {
    pub renderer: Renderer,
    pub lease: CapabilityLease,
    pub render_node: PathBuf,
    pub session: RendererSession,
}

impl RendererAccess {
    pub fn new(
        fd: OwnedFd,
        lease: CapabilityLease,
        render_node: PathBuf,
        session: RendererSession,
    ) -> Self {
        Self {
            fd,
            lease,
            render_node,
            session,
        }
    }

    pub fn render_node(&self) -> &Path {
        &self.render_node
    }

    /// Validate the endpoint without losing cleanup if the kernel rejects it.
    pub fn open(self) -> io::Result<OpenRenderer> {
        let renderer = Renderer::from_fd(self.fd)?;
        Ok(OpenRenderer {
            renderer,
            lease: self.lease,
            render_node: self.render_node,
            session: self.session,
        })
    }

    /// Return an unused endpoint without requiring it to describe an active mode.
    pub async fn release(self) -> io::Result<()> {
        drop(self.fd);
        self.lease.release().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    #[derive(Debug)]
    struct RejectingProvider(AtomicUsize);

    #[async_trait]
    impl RendererProvider for RejectingProvider {
        async fn acquire(
            &self,
            _: CancellationToken,
        ) -> Result<RendererAccess, RendererSessionError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(RendererSessionError::Unavailable)
        }
    }

    fn session() -> (RendererSession, Arc<RejectingProvider>) {
        let provider = Arc::new(RejectingProvider(AtomicUsize::new(0)));
        (RendererSession::new(provider.clone(), None), provider)
    }

    #[tokio::test]
    async fn authority_acquisition_does_not_require_migration() {
        let (session, provider) = session();
        assert!(matches!(
            session.acquire(CancellationToken::new()).await,
            Err(RendererSessionError::Unavailable)
        ));
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_migration_does_not_silently_install_a_transition() {
        let (session, provider) = session();
        assert!(matches!(
            session
                .install_transition(NonZeroU64::new(1).unwrap(), CancellationToken::new())
                .await,
            Err(RendererSessionError::MigrationUnavailable)
        ));
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelled_acquisition_does_not_contact_the_issuer() {
        let (session, provider) = session();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            session.acquire(cancellation).await,
            Err(RendererSessionError::Cancelled)
        ));
        assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejecting_an_endpoint_still_releases_its_issuer_resources() {
        let (session, _) = session();
        let (send, receive) = oneshot::channel();
        let access = RendererAccess::new(
            std::fs::File::open("/dev/null").unwrap().into(),
            CapabilityLease::new(async move {
                send.send(()).unwrap();
                Ok(())
            }),
            PathBuf::from("/dev/dri/renderD128"),
            session,
        );
        assert_eq!(
            access.open().unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        receive.await.unwrap();
    }

    #[derive(Debug, Clone)]
    struct ReturningProvider {
        releases: Arc<AtomicUsize>,
        cancel_before_reply: bool,
    }

    #[async_trait]
    impl RendererProvider for ReturningProvider {
        async fn acquire(
            &self,
            cancellation: CancellationToken,
        ) -> Result<RendererAccess, RendererSessionError> {
            let releases = Arc::clone(&self.releases);
            let access = RendererAccess::new(
                std::fs::File::open("/dev/null").unwrap().into(),
                CapabilityLease::new(async move {
                    releases.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                PathBuf::from("/dev/dri/renderD128"),
                RendererSession::new(Arc::new(self.clone()), None),
            );
            if self.cancel_before_reply {
                cancellation.cancel();
            }
            Ok(access)
        }
    }

    #[tokio::test]
    async fn a_provider_without_migration_can_issue_and_release_authority() {
        let releases = Arc::new(AtomicUsize::new(0));
        let session = RendererSession::new(
            Arc::new(ReturningProvider {
                releases: Arc::clone(&releases),
                cancel_before_reply: false,
            }),
            None,
        );
        let access = session.acquire(CancellationToken::new()).await.unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        assert_eq!(access.render_node(), Path::new("/dev/dri/renderD128"));
        access.release().await.unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_racing_a_ready_reply_releases_the_endpoint() {
        let releases = Arc::new(AtomicUsize::new(0));
        let session = RendererSession::new(
            Arc::new(ReturningProvider {
                releases: Arc::clone(&releases),
                cancel_before_reply: true,
            }),
            None,
        );
        assert!(matches!(
            session.acquire(CancellationToken::new()).await,
            Err(RendererSessionError::Cancelled)
        ));
        tokio::task::yield_now().await;
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }
}
