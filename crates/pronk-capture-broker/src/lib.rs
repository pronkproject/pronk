//! Session ownership for Mutter's private CastKMS display broker.
//!
//! Monitor control, renderer control and final-image capture arrive as separate
//! descriptors under one broker lifetime. Sessions confer no primary-node,
//! audio or CEC access. Release revokes every capability; it does not acknowledge
//! completion of admitted output writes.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
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

#[derive(Debug, thiserror::Error)]
pub enum RendererTransitionError {
    #[error("renderer transition installation canceled")]
    Cancelled,
    #[error("renderer transition installation timed out")]
    Timeout,
    #[error("renderer transition broker operation failed: {0}")]
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

/// Owns separate monitor, renderer and capture capabilities under one session.
///
/// Drop requests asynchronous release. Use [`Self::release`] to observe its
/// result. The Tokio runtime must remain alive for cleanup; process/bus-name
/// loss is the broker's fallback if that runtime stops. No control fd is exposed.
#[derive(Debug)]
pub struct Session {
    id: NonZeroU64,
    monitor: Option<OwnedFd>,
    renderer: Option<OwnedFd>,
    capture: Option<OwnedFd>,
    render_node: PathBuf,
    transition: RendererTransitionAccess,
    release: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<Result<(), Error>>>,
}

/// Capture authority derived from a display session's lifetime.
///
/// Cloning the underlying file description keeps capture available without
/// transferring monitor control or responsibility for releasing the broker
/// session.
#[derive(Debug)]
pub struct CaptureAccess {
    capture: OwnedFd,
}

/// Renderer authority derived from a display session's lifetime.
///
/// Cloning the underlying file description transfers no revocation authority,
/// monitor control, final-image capture access or primary-node operations.
#[derive(Debug)]
pub struct RendererAccess {
    renderer: OwnedFd,
    render_node: PathBuf,
    transition: RendererTransitionAccess,
}

/// Permission to ask Mutter to bind a registered profile to the current scene.
#[derive(Debug, Clone)]
pub struct RendererTransitionAccess {
    connection: zbus::Connection,
    owner: OwnedUniqueName,
    session_id: NonZeroU64,
    timeout: Duration,
}

impl RendererAccess {
    /// Render node for the GPU that produces the compositor's source images.
    pub fn render_node(&self) -> &Path {
        &self.render_node
    }

    /// Open a validated renderer client while retaining the broker session.
    pub fn open(&self) -> std::io::Result<castkms_renderer::Renderer> {
        castkms_renderer::Renderer::from_fd(self.renderer.try_clone()?)
    }

    /// Consume the capability and preserve its paired GPU selection.
    pub fn into_parts(
        self,
    ) -> std::io::Result<(
        castkms_renderer::Renderer,
        PathBuf,
        RendererTransitionAccess,
    )> {
        Ok((
            castkms_renderer::Renderer::from_fd(self.renderer)?,
            self.render_node,
            self.transition,
        ))
    }
}

impl RendererTransitionAccess {
    pub async fn install(
        &self,
        transition: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), RendererTransitionError> {
        let request = (self.session_id.get(), transition.get());
        let call = self.connection.call_method(
            Some(self.owner.as_str()),
            PATH,
            Some(SERVICE),
            "InstallRendererTransition",
            &request,
        );
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RendererTransitionError::Cancelled),
            result = tokio::time::timeout(self.timeout, call) => {
                result.map_err(|_| RendererTransitionError::Timeout)?
                    .and_then(|message| message.body().deserialize::<()>())
                    .map_err(RendererTransitionError::from)
            }
        }
    }
}

impl CaptureAccess {
    pub fn open(&self) -> std::io::Result<drm_capture::Client> {
        drm_capture::Client::from_fd(self.capture.try_clone()?)
    }
}

impl Session {
    fn capture(&self) -> BorrowedFd<'_> {
        self.capture
            .as_ref()
            .expect("live session owns capture")
            .as_fd()
    }

    fn renderer(&self) -> std::io::Result<BorrowedFd<'_>> {
        self.renderer
            .as_ref()
            .map(AsFd::as_fd)
            .ok_or_else(|| std::io::Error::other("renderer access was already transferred"))
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

    pub fn capture_access(&self) -> std::io::Result<CaptureAccess> {
        Ok(CaptureAccess {
            capture: self.capture().try_clone_to_owned()?,
        })
    }

    pub fn renderer_access(&self) -> std::io::Result<RendererAccess> {
        Ok(RendererAccess {
            renderer: self.renderer()?.try_clone_to_owned()?,
            render_node: self.render_node.clone(),
            transition: self.transition_access(),
        })
    }

    /// Transfer the session's renderer descriptor instead of retaining a duplicate.
    pub fn take_renderer_access(&mut self) -> std::io::Result<RendererAccess> {
        self.renderer
            .take()
            .map(|renderer| RendererAccess {
                renderer,
                render_node: self.render_node.clone(),
                transition: self.transition_access(),
            })
            .ok_or_else(|| std::io::Error::other("renderer access was already transferred"))
    }

    fn transition_access(&self) -> RendererTransitionAccess {
        self.transition.clone()
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

    /// Open a capture client while retaining the display session itself.
    ///
    /// Inactive or unauthorized outputs fail with the kernel's error and request
    /// session release. The client receives a close-on-exec duplicate of only
    /// the capture descriptor. Call this after display activation; it does not
    /// wait for a modeset or reserve the returned offer. Dropping the client
    /// leaves monitor control and broker ownership with the session.
    pub fn open_capture(&self) -> std::io::Result<drm_capture::Client> {
        self.capture_access()?.open()
    }

    /// Open a renderer client while retaining the display session itself.
    ///
    /// Call after display activation because monitor acquisition temporarily
    /// disconnects the output. The kernel rechecks the current master interval
    /// and exact enabled output when the client validates its descriptor.
    pub fn open_renderer(&self) -> std::io::Result<castkms_renderer::Renderer> {
        self.renderer_access()?.open()
    }

    pub async fn release(mut self) -> Result<(), Error> {
        self.monitor.take();
        self.renderer.take();
        self.capture.take();
        self.release.take();
        self.done
            .take()
            .expect("live session owns completion")
            .await
            .map_err(|_| Error::WorkerStopped)?
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.monitor.take();
        self.renderer.take();
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
            .deserialize::<(BusFd, BusFd, BusFd, String, u64)>()
    });
    let (monitor, renderer, capture, render_node, id) = match received {
        Ok((monitor, renderer, capture, render_node, id)) => {
            let Some(id) = NonZeroU64::new(id) else {
                let _ = send.send(Err(Error::InvalidSession));
                return;
            };
            (monitor, renderer, capture, PathBuf::from(render_node), id)
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
    // A rejected send drops the session here, waking the same cleanup path.
    let _ = send.send(Ok(Session {
        id,
        monitor: Some(monitor),
        renderer: Some(renderer),
        capture: Some(capture),
        render_node,
        transition: RendererTransitionAccess {
            connection: connection.clone(),
            owner: owner.clone(),
            session_id: id,
            timeout,
        },
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
            &(id.get(),),
        )
        .await
        .and_then(|message| message.body().deserialize::<()>());
    let _ = done.send(result.map_err(Error::from));
}

#[cfg(test)]
mod tests;
