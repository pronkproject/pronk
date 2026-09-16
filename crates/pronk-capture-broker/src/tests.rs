use super::*;
use std::io::{Read, Write};
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
    owner: Mutex<String>,
    entered: Notify,
    transition_entered: Notify,
    released: Notify,
    gate: Option<Notify>,
    transition_gate: Mutex<Option<Arc<Notify>>>,
    release_error: bool,
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
    ) -> zbus::fdo::Result<(BusFd, BusFd, BusFd, String, u64)> {
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
            capture.into(),
            "/dev/dri/renderD128".into(),
            91,
        ))
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

    fn release_display_session(
        &self,
        id: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
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
        .transition
        .install(NonZeroU64::new(73).unwrap(), CancellationToken::new())
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
    let mut capture = std::os::unix::net::UnixStream::from(capture_access.capture);
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
    let transition = session.renderer_access().unwrap().transition;
    let gate = Arc::new(Notify::new());
    *fixture.state.transition_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let cancellation = CancellationToken::new();
    let task = {
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            transition
                .install(NonZeroU64::new(74).unwrap(), cancellation)
                .await
        })
    };
    notified(&fixture.state.transition_entered).await;
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    cancellation.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(RendererTransitionError::Cancelled)
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
    let transition = session.renderer_access().unwrap().transition;
    let gate = Arc::new(Notify::new());
    *fixture.state.transition_gate.lock().unwrap() = Some(Arc::clone(&gate));
    assert!(matches!(
        transition
            .install(NonZeroU64::new(75).unwrap(), CancellationToken::new())
            .await,
        Err(RendererTransitionError::Timeout)
    ));
    gate.notify_one();
    transition_count_reaches(&fixture.state, 1).await;
    assert_eq!(
        fixture.state.transitions.lock().unwrap().as_slice(),
        &[(91, 75, ":1.88".into())]
    );
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

    fixture.renderer_peer.write_all(&[0x53]).unwrap();
    let mut renderer = std::os::unix::net::UnixStream::from(renderer.renderer);
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
