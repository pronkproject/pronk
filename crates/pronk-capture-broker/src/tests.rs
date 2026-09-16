use super::*;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::net::UnixStream;
use tokio::sync::Notify;
use zbus::connection::{AuthMechanism, Builder};
use zbus::message::Header;

type Request = (u32, u32, u32, u32, String);

#[derive(Default)]
struct State {
    requests: Mutex<Vec<Request>>,
    releases: Mutex<Vec<(u64, String)>>,
    transitions: Mutex<Vec<(u64, u64, String)>>,
    renderer_acquisitions: Mutex<Vec<(u64, u64, String)>>,
    renderer_releases: Mutex<Vec<(u64, u64, String)>>,
    renderer_peers: Mutex<BTreeMap<u64, std::os::unix::net::UnixStream>>,
    last_renderer_id: Mutex<u64>,
    renderer_acquire_calls: Mutex<usize>,
    renderer_acquire_entered: Notify,
    renderer_acquire_gate: Mutex<Option<Arc<Notify>>>,
    renderer_release_entered: Notify,
    renderer_release_gate: Mutex<Option<Arc<Notify>>>,
    owner: Mutex<String>,
    entered: Notify,
    transition_entered: Notify,
    released: Notify,
    release_entered: Notify,
    gate: Option<Notify>,
    release_gate: Mutex<Option<Arc<Notify>>>,
    transition_gate: Mutex<Option<Arc<Notify>>>,
    release_error: bool,
    invalid_renderer: AtomicBool,
}

struct Bus(Arc<State>);

#[zbus::interface(name = "org.freedesktop.DBus")]
impl Bus {
    fn get_name_owner(&self, name: &str) -> String {
        assert_eq!(name, SERVICE);
        self.0.owner.lock().unwrap().clone()
    }
}

struct Mutter {
    state: Arc<State>,
    monitor: Mutex<Option<OwnedFd>>,
    renderer: Mutex<Option<OwnedFd>>,
    capture: Mutex<Option<OwnedFd>>,
}

#[zbus::interface(name = "org.gnome.Mutter.CastKms")]
impl Mutter {
    async fn create_display_session(
        &self,
        major: u32,
        minor: u32,
        crtc: u32,
        connector: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(BusFd, BusFd, u64, BusFd, String, u64)> {
        self.state.requests.lock().unwrap().push((
            major,
            minor,
            crtc,
            connector,
            header.destination().unwrap().to_string(),
        ));
        self.state.entered.notify_one();
        if let Some(gate) = &self.state.gate {
            gate.notified().await;
        }
        let monitor = self
            .monitor
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| zbus::fdo::Error::Failed("monitor already issued".into()))?;
        let capture = self
            .capture
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| zbus::fdo::Error::Failed("capture already issued".into()))?;
        let renderer = self
            .renderer
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| zbus::fdo::Error::Failed("renderer already issued".into()))?;
        Ok((
            monitor.into(),
            renderer.into(),
            if self.state.invalid_renderer.load(Ordering::SeqCst) {
                0
            } else {
                1
            },
            capture.into(),
            "/dev/dri/renderD128".into(),
            91,
        ))
    }

    async fn acquire_renderer(
        &self,
        session_id: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(BusFd, u64)> {
        *self.state.renderer_acquire_calls.lock().unwrap() += 1;
        let gate = self.state.renderer_acquire_gate.lock().unwrap().clone();
        self.state.renderer_acquire_entered.notify_one();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let (renderer, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut last_id = self.state.last_renderer_id.lock().unwrap();
        *last_id += 1;
        let renderer_id = *last_id;
        self.state
            .renderer_peers
            .lock()
            .unwrap()
            .insert(renderer_id, peer);
        self.state.renderer_acquisitions.lock().unwrap().push((
            session_id,
            renderer_id,
            header.destination().unwrap().to_string(),
        ));
        let renderer: OwnedFd = renderer.into();
        Ok((renderer.into(), renderer_id))
    }

    async fn release_renderer(
        &self,
        session_id: u64,
        renderer_id: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let gate = self.state.renderer_release_gate.lock().unwrap().clone();
        self.state.renderer_release_entered.notify_one();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.state.renderer_releases.lock().unwrap().push((
            session_id,
            renderer_id,
            header.destination().unwrap().to_string(),
        ));
        if renderer_id == 0 || renderer_id > *self.state.last_renderer_id.lock().unwrap() {
            return Err(zbus::fdo::Error::InvalidArgs("unknown renderer".into()));
        }
        self.state
            .renderer_peers
            .lock()
            .unwrap()
            .remove(&renderer_id);
        Ok(())
    }

    async fn install_renderer_transition(
        &self,
        session_id: u64,
        transition: u64,
        #[zbus(header)] header: Header<'_>,
    ) {
        let destination = header.destination().unwrap().to_string();
        let gate = self.state.transition_gate.lock().unwrap().clone();
        self.state.transition_entered.notify_one();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.state
            .transitions
            .lock()
            .unwrap()
            .push((session_id, transition, destination));
    }

    async fn release_display_session(
        &self,
        id: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let gate = self.state.release_gate.lock().unwrap().clone();
        self.state.release_entered.notify_one();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.state
            .releases
            .lock()
            .unwrap()
            .push((id, header.destination().unwrap().to_string()));
        self.state.released.notify_one();
        if self.state.release_error {
            Err(zbus::fdo::Error::Failed("release rejected".into()))
        } else {
            Ok(())
        }
    }
}

struct Fixture {
    _server: zbus::Connection,
    monitor_peer: std::os::unix::net::UnixStream,
    renderer_peer: std::os::unix::net::UnixStream,
    _peer: std::os::unix::net::UnixStream,
    provider: Provider,
    state: Arc<State>,
}

impl Fixture {
    async fn new(stalled: bool, release_error: bool) -> Self {
        let state = Arc::new(State {
            owner: Mutex::new(":1.88".into()),
            last_renderer_id: Mutex::new(1),
            gate: stalled.then(Notify::new),
            release_error,
            ..State::default()
        });
        let (monitor, monitor_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (renderer, renderer_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (capture, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let server = Builder::unix_stream(server)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at("/org/freedesktop/DBus", Bus(Arc::clone(&state)))
            .unwrap()
            .serve_at(
                PATH,
                Mutter {
                    state: Arc::clone(&state),
                    monitor: Mutex::new(Some(monitor.into())),
                    renderer: Mutex::new(Some(renderer.into())),
                    capture: Mutex::new(Some(capture.into())),
                },
            )
            .unwrap();
        let client = Builder::unix_stream(client)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (server, connection) = tokio::try_join!(server.build(), client.build()).unwrap();
        Self {
            _server: server,
            monitor_peer,
            renderer_peer,
            _peer: peer,
            provider: Provider::new(
                connection,
                NonZeroUsize::new(1).unwrap(),
                Duration::from_secs(2),
            )
            .unwrap(),
            state,
        }
    }
}

fn target() -> Target {
    Target {
        device_major: 226,
        device_minor: 42,
        crtc_id: NonZeroU32::new(7).unwrap(),
        connector_id: NonZeroU32::new(11).unwrap(),
    }
}

async fn notified(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(2), notify.notified())
        .await
        .unwrap();
}

async fn transition_count_reaches(state: &State, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state.transitions.lock().unwrap().len() >= count {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn release_uses_the_issuing_owner_even_after_service_replacement() {
    let mut fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(session.id(), NonZeroU64::new(91).unwrap());
    fixture.monitor_peer.write_all(&[0x37]).unwrap();
    let mut monitor =
        std::os::unix::net::UnixStream::from(session.monitor().try_clone_to_owned().unwrap());
    monitor
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut monitor_byte = [0];
    monitor.read_exact(&mut monitor_byte).unwrap();
    assert_eq!(monitor_byte, [0x37]);
    drop(monitor);
    fixture.renderer_peer.write_all(&[0x41]).unwrap();
    let renderer_access = session.renderer_access().unwrap();
    assert_eq!(
        renderer_access.render_node(),
        std::path::Path::new("/dev/dri/renderD128")
    );
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    renderer_access
        .session
        .install_transition(NonZeroU64::new(73).unwrap(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fixture.state.transitions.lock().unwrap().as_slice(),
        &[(91, 73, ":1.88".into())]
    );
    let mut renderer = std::os::unix::net::UnixStream::from(renderer_access.renderer);
    renderer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut renderer_byte = [0];
    renderer.read_exact(&mut renderer_byte).unwrap();
    assert_eq!(renderer_byte, [0x41]);
    drop(renderer);
    fixture._peer.write_all(&[0x49]).unwrap();
    let capture_access = session.capture_access().unwrap();
    let mut capture = std::os::unix::net::UnixStream::from(capture_access.into_fd());
    capture
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut pixel = [0];
    capture.read_exact(&mut pixel).unwrap();
    assert_eq!(pixel, [0x49]);
    drop(capture);
    assert_eq!(
        fixture.state.requests.lock().unwrap().as_slice(),
        &[(226, 42, 7, 11, ":1.88".into())]
    );
    session.release().await.unwrap();
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
    assert_eq!(fixture.provider.slots.available_permits(), 1);
    fixture
        .monitor_peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fixture.monitor_peer.read(&mut [0]).unwrap(), 0);
    fixture
        .renderer_peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fixture.renderer_peer.read(&mut [0]).unwrap(), 0);
}

#[tokio::test]
async fn cancelling_a_stalled_transition_returns_without_changing_owners() {
    let fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let transition = session.renderer_access().unwrap().session;
    let gate = Arc::new(Notify::new());
    *fixture.state.transition_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let cancellation = CancellationToken::new();
    let task = {
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            transition
                .install_transition(NonZeroU64::new(74).unwrap(), cancellation)
                .await
        })
    };
    notified(&fixture.state.transition_entered).await;
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    cancellation.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(RendererSessionError::Cancelled)
    ));

    gate.notify_one();
    transition_count_reaches(&fixture.state, 1).await;
    assert_eq!(
        fixture.state.transitions.lock().unwrap().as_slice(),
        &[(91, 74, ":1.88".into())]
    );
}

#[tokio::test]
async fn a_stalled_transition_obeys_the_session_deadline() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.provider.timeout = Duration::from_millis(20);
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let transition = session.renderer_access().unwrap().session;
    let gate = Arc::new(Notify::new());
    *fixture.state.transition_gate.lock().unwrap() = Some(Arc::clone(&gate));
    assert!(matches!(
        transition
            .install_transition(NonZeroU64::new(75).unwrap(), CancellationToken::new())
            .await,
        Err(RendererSessionError::Timeout)
    ));
    gate.notify_one();
    transition_count_reaches(&fixture.state, 1).await;
    assert_eq!(
        fixture.state.transitions.lock().unwrap().as_slice(),
        &[(91, 75, ":1.88".into())]
    );
}

#[tokio::test]
async fn rejecting_renderer_metadata_releases_the_issued_display_session() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.state.invalid_renderer.store(true, Ordering::SeqCst);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::InvalidRenderer)
    ));
    notified(&fixture.state.released).await;
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
    for peer in [
        &mut fixture.monitor_peer,
        &mut fixture.renderer_peer,
        &mut fixture._peer,
    ] {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[tokio::test]
async fn dropping_a_session_requests_release() {
    let mut fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    drop(session);
    notified(&fixture.state.released).await;
    assert_eq!(fixture.state.releases.lock().unwrap().len(), 1);
    fixture
        .monitor_peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fixture.monitor_peer.read(&mut [0]).unwrap(), 0);
    fixture
        .renderer_peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fixture.renderer_peer.read(&mut [0]).unwrap(), 0);
}

#[tokio::test]
async fn renderer_access_can_move_out_of_the_broker_session_once() {
    let mut fixture = Fixture::new(false, false).await;
    let mut session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let renderer = session.take_renderer_access().unwrap();
    assert!(session.take_renderer_access().is_err());
    assert!(session.renderer_access().is_err());
    let (descriptor, endpoint_id, render_node, issuer) = renderer.into_capability();
    assert_eq!(endpoint_id.get(), 1);
    assert_eq!(render_node, Path::new("/dev/dri/renderD128"));
    assert_eq!(issuer.session_id, session.id());

    fixture.renderer_peer.write_all(&[0x53]).unwrap();
    let mut renderer = std::os::unix::net::UnixStream::from(descriptor);
    renderer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0];
    renderer.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0x53]);

    session.release().await.unwrap();
    fixture
        .renderer_peer
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    assert!(fixture.renderer_peer.read(&mut [0]).is_err());
    drop(renderer);
    assert_eq!(fixture.renderer_peer.read(&mut [0]).unwrap(), 0);
}

#[tokio::test]
async fn fresh_renderer_endpoints_are_released_through_the_issuing_owner() {
    let fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let renderer_session = session.renderer_access().unwrap().session;
    *fixture.state.owner.lock().unwrap() = ":1.99".into();

    let access = renderer_session
        .acquire_renderer(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(access.endpoint_id, NonZeroU64::new(2).unwrap());
    assert_eq!(
        fixture
            .state
            .renderer_acquisitions
            .lock()
            .unwrap()
            .as_slice(),
        &[(91, 2, ":1.88".into())]
    );
    let endpoint_id = access.endpoint_id;
    drop(access.renderer);
    renderer_session
        .release_renderer(endpoint_id)
        .await
        .unwrap();
    renderer_session
        .release_renderer(endpoint_id)
        .await
        .unwrap();
    assert_eq!(
        fixture.state.renderer_releases.lock().unwrap().as_slice(),
        &[(91, 2, ":1.88".into()), (91, 2, ":1.88".into())]
    );
}

#[tokio::test]
async fn timed_out_renderer_acquisition_keeps_one_request_until_cleanup() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.provider.timeout = Duration::from_millis(20);
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let renderer_session = session.renderer_access().unwrap().session;
    let gate = Arc::new(Notify::new());
    *fixture.state.renderer_acquire_gate.lock().unwrap() = Some(Arc::clone(&gate));

    let first = {
        let renderer_session = renderer_session.clone();
        tokio::spawn(async move {
            renderer_session
                .acquire_renderer(CancellationToken::new())
                .await
        })
    };
    notified(&fixture.state.renderer_acquire_entered).await;
    assert!(matches!(
        first.await.unwrap(),
        Err(RendererSessionError::Timeout)
    ));
    assert!(matches!(
        renderer_session
            .acquire_renderer(CancellationToken::new())
            .await,
        Err(RendererSessionError::Timeout)
    ));
    assert_eq!(*fixture.state.renderer_acquire_calls.lock().unwrap(), 1);

    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !fixture.state.renderer_releases.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.state.renderer_releases.lock().unwrap().as_slice(),
        &[(91, 2, ":1.88".into())]
    );
}

struct AcquisitionWake(Notify);

impl std::task::Wake for AcquisitionWake {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_one();
    }
}

async fn abandon_ready_renderer_reply(cancel: bool) {
    use std::future::Future;
    use std::task::{Context, Waker};

    let fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let renderer_session = session.renderer_access().unwrap().session;
    let cancellation = CancellationToken::new();
    let mut acquisition = Box::pin(renderer_session.acquire_renderer(cancellation.clone()));
    let wake = Arc::new(AcquisitionWake(Notify::new()));
    let waker = Waker::from(Arc::clone(&wake));
    assert!(acquisition
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());

    // Keep the caller unpolled until its reply has reached the local channel.
    tokio::time::timeout(Duration::from_secs(1), wake.0.notified())
        .await
        .unwrap();
    assert_eq!(fixture.state.renderer_acquisitions.lock().unwrap().len(), 1);
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    if cancel {
        cancellation.cancel();
        assert!(matches!(
            acquisition.await,
            Err(RendererSessionError::Cancelled)
        ));
    } else {
        drop(acquisition);
    }
    notified(&fixture.state.renderer_release_entered).await;
    assert_eq!(
        fixture.state.renderer_releases.lock().unwrap().as_slice(),
        &[(91, 2, ":1.88".into())]
    );
    session.release().await.unwrap();
}

#[tokio::test]
async fn cancellation_releases_an_unclaimed_ready_renderer_reply() {
    abandon_ready_renderer_reply(true).await;
}

#[tokio::test]
async fn abandoning_an_unclaimed_ready_renderer_reply_requests_release() {
    abandon_ready_renderer_reply(false).await;
}

#[tokio::test]
async fn timed_out_renderer_release_blocks_replacement_until_cleanup() {
    let mut fixture = Fixture::new(false, false).await;
    fixture.provider.timeout = Duration::from_millis(50);
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    let renderer_session = session.renderer_access().unwrap().session;
    let access = renderer_session
        .acquire_renderer(CancellationToken::new())
        .await
        .unwrap();
    let endpoint_id = access.endpoint_id;
    drop(access);
    let gate = Arc::new(Notify::new());
    *fixture.state.renderer_release_gate.lock().unwrap() = Some(Arc::clone(&gate));

    let release = {
        let renderer_session = renderer_session.clone();
        tokio::spawn(async move { renderer_session.release_renderer(endpoint_id).await })
    };
    notified(&fixture.state.renderer_release_entered).await;
    assert!(matches!(
        release.await.unwrap(),
        Err(RendererSessionError::Timeout)
    ));
    assert!(matches!(
        renderer_session
            .acquire_renderer(CancellationToken::new())
            .await,
        Err(RendererSessionError::Timeout)
    ));
    assert_eq!(*fixture.state.renderer_acquire_calls.lock().unwrap(), 1);

    *fixture.state.renderer_release_gate.lock().unwrap() = None;
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !fixture.state.renderer_releases.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replacement = renderer_session
        .acquire_renderer(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(replacement.endpoint_id, NonZeroU64::new(3).unwrap());
    let replacement_id = replacement.endpoint_id;
    drop(replacement);
    renderer_session
        .release_renderer(replacement_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_before_acquisition_does_not_send_a_request() {
    let fixture = Fixture::new(false, false).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        fixture.provider.acquire(target(), cancellation).await,
        Err(Error::Cancelled)
    ));
    assert!(fixture.state.requests.lock().unwrap().is_empty());
    assert_eq!(fixture.provider.slots.available_permits(), 1);
}

#[tokio::test]
async fn cancellation_drains_and_releases_a_late_reply() {
    let fixture = Fixture::new(true, false).await;
    let cancellation = CancellationToken::new();
    let task = {
        let provider = fixture.provider.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move { provider.acquire(target(), cancellation).await })
    };
    notified(&fixture.state.entered).await;
    cancellation.cancel();
    assert!(matches!(task.await.unwrap(), Err(Error::Cancelled)));
    assert_eq!(fixture.provider.slots.available_permits(), 0);
    fixture.state.gate.as_ref().unwrap().notify_one();
    notified(&fixture.state.released).await;
}

#[tokio::test]
async fn dropping_the_acquisition_future_also_drains_the_reply() {
    let fixture = Fixture::new(true, false).await;
    let task = {
        let provider = fixture.provider.clone();
        tokio::spawn(async move { provider.acquire(target(), CancellationToken::new()).await })
    };
    notified(&fixture.state.entered).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    fixture.state.gate.as_ref().unwrap().notify_one();
    notified(&fixture.state.released).await;
}

#[tokio::test]
async fn timed_out_requests_remain_bounded_until_the_reply_is_drained() {
    let mut fixture = Fixture::new(true, false).await;
    fixture.provider.timeout = Duration::from_millis(100);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::Timeout)
    ));
    assert_eq!(fixture.provider.slots.available_permits(), 0);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::Timeout)
    ));
    assert_eq!(fixture.state.requests.lock().unwrap().len(), 1);
    fixture.state.gate.as_ref().unwrap().notify_one();
    notified(&fixture.state.released).await;
}

#[tokio::test]
async fn explicit_release_reports_a_broker_error() {
    let fixture = Fixture::new(false, true).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(session.release().await, Err(Error::Bus(_))));
}

#[tokio::test]
async fn a_stalled_display_release_bounds_the_wait_without_freeing_capacity() {
    let mut fixture = Fixture::new(false, false).await;
    let mut session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    session.timeout = Duration::from_millis(20);
    fixture.provider.timeout = Duration::from_millis(20);
    let gate = Arc::new(Notify::new());
    *fixture.state.release_gate.lock().unwrap() = Some(Arc::clone(&gate));
    *fixture.state.owner.lock().unwrap() = ":1.99".into();

    let release = tokio::spawn(session.release());
    notified(&fixture.state.release_entered).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), release)
            .await
            .expect("a broker release must not wait indefinitely")
            .unwrap(),
        Err(Error::Timeout)
    ));
    assert_eq!(fixture.provider.slots.available_permits(), 0);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::Timeout)
    ));
    assert_eq!(fixture.state.requests.lock().unwrap().len(), 1);
    for peer in [
        &mut fixture.monitor_peer,
        &mut fixture.renderer_peer,
        &mut fixture._peer,
    ] {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    gate.notify_one();
    notified(&fixture.state.released).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.provider.slots.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
}

#[tokio::test]
async fn rejecting_a_non_capture_descriptor_releases_the_session() {
    let mut fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    // The fixture transfers a socket, not an anonymous DRM capture file.
    assert!(session.open_capture().is_err());
    drop(session);
    notified(&fixture.state.released).await;
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
    fixture
        ._peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fixture._peer.read(&mut [0]).unwrap(), 0);
    fixture
        .monitor_peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fixture.monitor_peer.read(&mut [0]).unwrap(), 0);
}
