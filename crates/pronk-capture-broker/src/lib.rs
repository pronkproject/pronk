//! Session ownership for Mutter's private CastKMS display broker.
//!
//! Monitor control and final-image capture arrive as separate descriptors under
//! one broker lifetime. Sessions confer no primary-node, renderer, audio or CEC
//! access. Release revokes the broker capabilities; it does not acknowledge
//! completion of admitted output writes.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::OwnedFd as BusFd;

const SERVICE: &str = "org.gnome.Mutter.CastKms";
const PATH: &str = "/org/gnome/Mutter/CastKms";

/// An exact output on a device owned by the compositor.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub device_major: u32,
    pub device_minor: u32,
    pub crtc_id: NonZeroU32,
    pub connector_id: NonZeroU32,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("display-session acquisition canceled")]
    Cancelled,
    #[error("display-session operation timed out")]
    Timeout,
    #[error("display-session worker stopped")]
    WorkerStopped,
    #[error("the shared bus connection must not impose a method timeout")]
    ConnectionTimeout,
    #[error("display-session limit exceeds semaphore capacity")]
    InvalidCapacity,
    #[error("Mutter returned a zero display-session identifier")]
    InvalidSession,
    #[error("display-session broker operation failed: {0}")]
    Bus(#[from] zbus::Error),
}

/// Clones share a limit on pending, active and retiring sessions.
///
/// A stalled reply retains one slot, not an unbounded detached task. The caller's
/// deadline does not cancel the bus call: a late successful reply must be released.
#[derive(Debug, Clone)]
pub struct Provider {
    connection: zbus::Connection,
    slots: Arc<Semaphore>,
    timeout: Duration,
}

/// Owns separate monitor and capture capabilities under one session.
///
/// Drop requests asynchronous release. Use [`Self::release`] to observe its
/// result. The Tokio runtime must remain alive for cleanup; process/bus-name
/// loss is the broker's fallback if that runtime stops. No control fd is exposed.
#[derive(Debug)]
pub struct Session {
    id: NonZeroU64,
    target: Target,
    timeout: Duration,
    monitor: OwnedFd,
    capture: OwnedFd,
    release: oneshot::Sender<()>,
    done: oneshot::Receiver<Result<(), Error>>,
}

impl Session {
    fn capture(&self) -> BorrowedFd<'_> {
        self.capture.as_fd()
    }

    /// Borrow the monitor-control capability without exposing capture through it.
    pub fn monitor(&self) -> BorrowedFd<'_> {
        self.monitor.as_fd()
    }

    pub fn id(&self) -> NonZeroU64 {
        self.id
    }

    /// Exact output retained by this display-session authorization.
    ///
    /// This target is an output identity, not kernel authority.
    pub fn target(&self) -> Target {
        self.target
    }

    pub fn capture_access(&self) -> std::io::Result<drm_capture::Access> {
        Ok(drm_capture::Access::from_fd(
            self.capture().try_clone_to_owned()?,
        ))
    }

    pub fn monitor_capabilities(&self) -> std::io::Result<castkms_monitor::Capabilities> {
        castkms_monitor::query_capabilities(self.monitor())
    }

    pub fn attach_monitor(&self, edid: Option<&[u8]>) -> std::io::Result<()> {
        castkms_monitor::attach_monitor(self.monitor(), edid)
    }

    pub fn detach_monitor(&self) -> std::io::Result<()> {
        castkms_monitor::detach_monitor(self.monitor())
    }

    /// Open a capture client while retaining the display session itself.
    ///
    /// Inactive or unauthorized outputs fail with the kernel's error without
    /// releasing the session. The client receives a close-on-exec duplicate of
    /// only the capture descriptor. Call this after display activation; it does not
    /// wait for a modeset or reserve the returned offer. Dropping the client
    /// leaves monitor control and broker ownership with the session.
    pub fn open_capture(&self) -> std::io::Result<drm_capture::Client> {
        self.capture_access()?.open()
    }

    /// Close local capabilities and observe release within the session deadline.
    ///
    /// Timeout stops the local wait, not the issuer's release operation. The
    /// provider retains its capacity until that operation finishes.
    pub async fn release(self) -> Result<(), Error> {
        let Self {
            timeout,
            monitor,
            capture,
            release,
            done,
            ..
        } = self;
        drop(monitor);
        drop(capture);
        drop(release);
        tokio::time::timeout(timeout, done)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::WorkerStopped)?
    }
}

impl Provider {
    pub fn new(
        connection: zbus::Connection,
        max_sessions: NonZeroUsize,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if connection.method_timeout().is_some() {
            return Err(Error::ConnectionTimeout);
        }
        if max_sessions.get() > Semaphore::MAX_PERMITS {
            return Err(Error::InvalidCapacity);
        }
        Ok(Self {
            connection,
            slots: Arc::new(Semaphore::new(max_sessions.get())),
            timeout,
        })
    }

    pub async fn acquire(
        &self,
        target: Target,
        cancellation: CancellationToken,
    ) -> Result<Session, Error> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(self.timeout, self.acquire_inner(target)) => {
                result.map_err(|_| Error::Timeout)?
            }
        }
    }

    async fn acquire_inner(&self, target: Target) -> Result<Session, Error> {
        let permit = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .map_err(|_| Error::WorkerStopped)?;
        let connection = self.connection.clone();
        let timeout = self.timeout;
        let (send, receive) = oneshot::channel();
        tokio::spawn(async move {
            let _permit = permit;
            let owner = match resolve_owner(&connection).await {
                Ok(owner) => owner,
                Err(error) => {
                    let _ = send.send(Err(error));
                    return;
                }
            };
            run_session(connection, owner, target, timeout, send).await;
        });
        receive.await.map_err(|_| Error::WorkerStopped)?
    }
}

async fn resolve_owner(connection: &zbus::Connection) -> Result<OwnedUniqueName, Error> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    bus.get_name_owner(SERVICE.try_into().expect("constant bus name"))
        .await
        .map_err(|error| Error::Bus(error.into()))
}

async fn run_session(
    connection: zbus::Connection,
    owner: OwnedUniqueName,
    target: Target,
    timeout: Duration,
    send: oneshot::Sender<Result<Session, Error>>,
) {
    let result = connection
        .call_method(
            Some(owner.as_str()),
            PATH,
            Some(SERVICE),
            "CreateDisplaySession",
            &(
                target.device_major,
                target.device_minor,
                target.crtc_id.get(),
                target.connector_id.get(),
            ),
        )
        .await;
    let received = result.and_then(|message| message.body().deserialize::<(BusFd, BusFd, u64)>());
    let (monitor, capture, id) = match received {
        Ok((monitor, capture, id)) => {
            let Some(id) = NonZeroU64::new(id) else {
                let _ = send.send(Err(Error::InvalidSession));
                return;
            };
            (monitor, capture, id)
        }
        Err(error) => {
            let _ = send.send(Err(error.into()));
            return;
        }
    };
    let monitor: OwnedFd = monitor.into();
    let capture: OwnedFd = capture.into();
    let (release, wait_release) = oneshot::channel();
    let (done, wait_done) = oneshot::channel();
    // Validation owns the descriptors and release trigger. Rejection closes
    // them and follows the same cleanup path as an unclaimed successful reply.
    let session = Session {
        id,
        target,
        timeout,
        monitor,
        capture,
        release,
        done: wait_done,
    };
    let _ = send.send(Ok(session));
    let _ = wait_release.await;
    let result = connection
        .call_method(
            Some(owner.as_str()),
            PATH,
            Some(SERVICE),
            "ReleaseDisplaySession",
            &(id.get(),),
        )
        .await
        .and_then(|message| message.body().deserialize::<()>());
    let _ = done.send(result.map_err(Error::from));
}

#[cfg(test)]
mod tests;
