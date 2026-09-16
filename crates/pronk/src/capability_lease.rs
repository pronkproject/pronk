//! Exactly-once cleanup for authority issued by another component.

use std::future::Future;
use std::io;

use tokio::sync::oneshot;

/// Owns the obligation to return one issued capability.
///
/// Drop requests cleanup; [`Self::release`] additionally observes its result.
/// Abandoning that wait does not abandon cleanup. The Tokio runtime must remain
/// alive for the worker to finish. Release does not prove that GPU access ended:
/// the capability's caller must drain admitted work before orderly release.
#[derive(Debug)]
pub struct CapabilityLease {
    release: oneshot::Sender<()>,
    done: oneshot::Receiver<io::Result<()>>,
}

impl CapabilityLease {
    /// Arrange cleanup on the current Tokio runtime without starting it yet.
    pub fn new(cleanup: impl Future<Output = io::Result<()>> + Send + 'static) -> Self {
        let (release, wait) = oneshot::channel();
        let (done, result) = oneshot::channel();
        tokio::spawn(async move {
            let _ = wait.await;
            if let Err(Err(error)) = done.send(cleanup.await) {
                tracing::warn!(%error, "capability cleanup failed after its owner stopped waiting");
            }
        });
        Self {
            release,
            done: result,
        }
    }

    pub async fn release(self) -> io::Result<()> {
        let Self { release, done } = self;
        drop(release);
        done.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "capability cleanup worker stopped",
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn cleanup_begins_only_when_the_owner_releases() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let lease = CapabilityLease::new(async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        lease.release().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dropping_the_owner_still_requests_cleanup() {
        let (send, receive) = oneshot::channel();
        let lease = CapabilityLease::new(async move {
            send.send(()).unwrap();
            Ok(())
        });
        drop(lease);
        receive.await.unwrap();
    }

    #[tokio::test]
    async fn abandoning_explicit_release_does_not_cancel_cleanup() {
        let (entered, wait_entered) = oneshot::channel();
        let (resume, wait_resume) = oneshot::channel();
        let (finished, wait_finished) = oneshot::channel();
        let lease = CapabilityLease::new(async move {
            entered.send(()).unwrap();
            wait_resume.await.unwrap();
            finished.send(()).unwrap();
            Ok(())
        });
        let task = tokio::spawn(lease.release());
        wait_entered.await.unwrap();
        task.abort();
        let _ = task.await;
        resume.send(()).unwrap();
        wait_finished.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_release_preserves_cleanup_failure() {
        let lease =
            CapabilityLease::new(async { Err(io::Error::from_raw_os_error(nix::libc::EIO)) });
        assert_eq!(
            lease.release().await.unwrap_err().raw_os_error(),
            Some(nix::libc::EIO)
        );
    }
}
