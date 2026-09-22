//! Session ownership for Mutter's private CastKMS display broker.
//!
//! Monitor control, final-image capture, and an initial renderer endpoint arrive
//! as separate descriptors under one broker lifetime. The renderer issuer is
//! bound to that exact session and can issue a replacement only after the worker
//! retires the prior endpoint. Sessions confer no primary-node, audio or CEC
//! access. Release revokes the broker capabilities; it does not acknowledge
//! completion of admitted output writes.

use std::io;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
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
    #[error("Mutter returned a zero renderer identifier")]
    InvalidRenderer,
    #[error("Mutter returned an invalid render node")]
    InvalidRenderNode,
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
    monitor: Option<OwnedFd>,
    capture: Option<OwnedFd>,
    renderer: Option<RendererEndpoint>,
    release: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<Result<(), Error>>>,
}

/// One renderer descriptor issued for a display session.
#[derive(Debug)]
pub struct RendererEndpoint {
    fd: OwnedFd,
    id: NonZeroU64,
    issuer: RendererIssuer,
}

/// Bounded renderer issuance for one exact broker display session.
#[derive(Debug, Clone)]
pub struct RendererIssuer {
    connection: zbus::Connection,
    owner: OwnedUniqueName,
    session_id: NonZeroU64,
    render_node: PathBuf,
    timeout: Duration,
}

impl Session {
    fn capture(&self) -> BorrowedFd<'_> {
        self.capture
            .as_ref()
            .expect("live session owns capture")
            .as_fd()
    }

    /// Borrow the monitor-control capability without exposing capture through it.
    pub fn monitor(&self) -> BorrowedFd<'_> {
        self.monitor
            .as_ref()
            .expect("live session owns monitor control")
            .as_fd()
    }

    pub fn id(&self) -> NonZeroU64 {
        self.id
    }

    /// Exact output retained by this display-session authorization.
    ///
    /// A separate privileged renderer issuer must receive this same target;
    /// the session ID alone is neither kernel authority nor an output identity.
    pub fn target(&self) -> Target {
        self.target
    }

    /// Transfer the initial renderer endpoint and its session-bound issuer.
    pub fn take_renderer(&mut self) -> io::Result<RendererEndpoint> {
        self.renderer.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the display session has no renderer endpoint",
            )
        })
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
    pub async fn release(mut self) -> Result<(), Error> {
        self.monitor.take();
        self.capture.take();
        self.renderer.take();
        self.release.take();
        let done = self.done.take().expect("live session owns completion");
        tokio::time::timeout(self.timeout, done)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::WorkerStopped)?
    }
}

impl RendererEndpoint {
    /// Split the endpoint from the broker issuer that must revoke this ID.
    pub fn into_parts(self) -> (OwnedFd, NonZeroU64, RendererIssuer) {
        (self.fd, self.id, self.issuer)
    }
}

impl RendererIssuer {
    pub fn render_node(&self) -> &Path {
        &self.render_node
    }

    /// Issue a fresh endpoint after an earlier renderer generation retires.
    pub async fn acquire(&self) -> Result<RendererEndpoint, Error> {
        tokio::time::timeout(self.timeout, self.acquire_inner())
            .await
            .map_err(|_| Error::Timeout)?
    }

    async fn acquire_inner(&self) -> Result<RendererEndpoint, Error> {
        let result = self
            .connection
            .call_method(
                Some(self.owner.as_str()),
                PATH,
                Some(SERVICE),
                "AcquireRenderer",
                &(self.session_id.get(),),
            )
            .await;
        let (fd, id) = result.and_then(|message| message.body().deserialize::<(BusFd, u64)>())?;
        let id = NonZeroU64::new(id).ok_or(Error::InvalidRenderer)?;
        Ok(RendererEndpoint {
            fd: fd.into(),
            id,
            issuer: self.clone(),
        })
    }

    /// Revoke an endpoint after its local descriptor and admitted work drain.
    pub async fn release(&self, renderer_id: NonZeroU64) -> Result<(), Error> {
        tokio::time::timeout(self.timeout, async {
            self.connection
                .call_method(
                    Some(self.owner.as_str()),
                    PATH,
                    Some(SERVICE),
                    "ReleaseRenderer",
                    &(self.session_id.get(), renderer_id.get()),
                )
                .await
                .and_then(|message| message.body().deserialize::<()>())
                .map_err(Error::from)
        })
        .await
        .map_err(|_| Error::Timeout)?
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.monitor.take();
        self.capture.take();
        self.release.take();
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
    let received = result.and_then(|message| {
        message
            .body()
            .deserialize::<(BusFd, BusFd, u64, BusFd, String, u64)>()
    });
    let (monitor, renderer, renderer_id, capture, render_node, id) = match received {
        Ok((monitor, renderer, renderer_id, capture, render_node, id)) => {
            let Some(id) = NonZeroU64::new(id) else {
                let _ = send.send(Err(Error::InvalidSession));
                return;
            };
            let Some(renderer_id) = NonZeroU64::new(renderer_id) else {
                let _ = send.send(Err(Error::InvalidRenderer));
                return;
            };
            let render_node = PathBuf::from(render_node);
            if !render_node.is_absolute() {
                let _ = send.send(Err(Error::InvalidRenderNode));
                return;
            }
            (monitor, renderer, renderer_id, capture, render_node, id)
        }
        Err(error) => {
            let _ = send.send(Err(error.into()));
            return;
        }
    };
    let monitor: OwnedFd = monitor.into();
    let renderer: OwnedFd = renderer.into();
    let capture: OwnedFd = capture.into();
    let (release, wait_release) = oneshot::channel();
    let (done, wait_done) = oneshot::channel();
    // Validation owns the descriptors and release trigger. Rejection closes
    // them and follows the same cleanup path as an unclaimed successful reply.
    let session = Session {
        id,
        target,
        timeout,
        monitor: Some(monitor),
        capture: Some(capture),
        renderer: Some(RendererEndpoint {
            fd: renderer,
            id: renderer_id,
            issuer: RendererIssuer {
                connection: connection.clone(),
                owner: owner.clone(),
                session_id: id,
                render_node,
                timeout,
            },
        }),
        release: Some(release),
        done: Some(wait_done),
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
