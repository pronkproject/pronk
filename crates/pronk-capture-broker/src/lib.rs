//! Session ownership for Mutter's private CastKMS display broker.
//!
//! Monitor control and final-image capture arrive as separate descriptors under
//! one broker lifetime. Sessions confer no primary-node, audio, CEC or raw-plane
//! access. Release revokes both capabilities; it does not acknowledge completion
//! of admitted output writes.

use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::OwnedFd as BusFd;

mod monitor;

pub use monitor::Capabilities as MonitorCapabilities;

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
    #[error("display-session acquisition timed out")]
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

/// Owns separate monitor and capture capabilities under one broker session.
///
/// Drop requests asynchronous release. Use [`Self::release`] to observe its
/// result. The Tokio runtime must remain alive for cleanup; process/bus-name
/// loss is the broker's fallback if that runtime stops. No control fd is exposed.
#[derive(Debug)]
pub struct Session {
    monitor: Option<OwnedFd>,
    capture: Option<OwnedFd>,
    release: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<Result<(), Error>>>,
}

impl AsFd for Session {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.capture
            .as_ref()
            .expect("live session owns capture")
            .as_fd()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.monitor.take();
        self.capture.take();
        self.release.take();
    }
}

impl Session {
    /// Borrow the monitor-control capability without exposing capture through it.
    pub fn monitor(&self) -> BorrowedFd<'_> {
        self.monitor
            .as_ref()
            .expect("live session owns monitor control")
            .as_fd()
    }

    pub fn monitor_capabilities(&self) -> std::io::Result<monitor::Capabilities> {
        monitor::query_capabilities(self.monitor())
    }

    pub fn attach_monitor(&self, edid: Option<&[u8]>) -> std::io::Result<()> {
        monitor::attach_monitor(self.monitor(), edid)
    }

    pub fn detach_monitor(&self) -> std::io::Result<()> {
        monitor::detach_monitor(self.monitor())
    }

    /// Validate the current image offer and move the whole session into the client.
    ///
    /// Inactive or unauthorized outputs fail with the kernel's error and request
    /// session release. The monitor-control descriptor remains owned but is not
    /// exposed through the DRM capture client. Call this after display activation;
    /// it does not wait for a modeset or reserve the returned offer. Dropping the
    /// client requests release even if no stream was opened. For observed release,
    /// recover the session with `Client::into_owner` and call [`Self::release`].
    /// Neither path acknowledges completion of outstanding destination writes.
    pub fn into_capture(self) -> std::io::Result<drm_capture::Client<Self>> {
        drm_capture::Client::from_owner(self)
    }

    pub async fn release(mut self) -> Result<(), Error> {
        self.monitor.take();
        self.capture.take();
        self.release.take();
        self.done
            .take()
            .expect("live session owns completion")
            .await
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
            run_session(connection, owner, target, send).await;
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
        Ok((monitor, capture, id)) if id != 0 => (monitor, capture, id),
        Ok(_) => {
            let _ = send.send(Err(Error::InvalidSession));
            return;
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
    // A rejected send drops the session here, waking the same cleanup path.
    let _ = send.send(Ok(Session {
        monitor: Some(monitor),
        capture: Some(capture),
        release: Some(release),
        done: Some(wait_done),
    }));
    let _ = wait_release.await;
    let result = connection
        .call_method(
            Some(owner.as_str()),
            PATH,
            Some(SERVICE),
            "ReleaseDisplaySession",
            &(id,),
        )
        .await
        .and_then(|message| message.body().deserialize::<()>());
    let _ = done.send(result.map_err(Error::from));
}

#[cfg(test)]
mod tests;
