use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::net::UnixStream;
use tokio::sync::Notify;
use zbus::connection::{AuthMechanism, Builder};
use zbus::message::Header;

#[derive(Default)]
struct State {
    owner: Mutex<String>,
    requests: Mutex<usize>,
    releases: Mutex<Vec<(u64, String)>>,
    renderer_releases: Mutex<Vec<(u64, u64, String)>>,
    entered: Notify,
    released: Notify,
    gate: Mutex<Option<Arc<Notify>>>,
    release_entered: Notify,
    release_gate: Mutex<Option<Arc<Notify>>>,
    release_error: AtomicBool,
    invalid_id: AtomicBool,
    invalid_renderer_id: AtomicBool,
    invalid_render_node: AtomicBool,
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
    ) -> zbus::fdo::Result<(BusFd, BusFd, u64, BusFd, String, u64)> {
        assert_eq!((major, minor, crtc, connector), (226, 42, 7, 11));
        *self.state.requests.lock().unwrap() += 1;
        self.state.entered.notify_one();
        let gate = self.state.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let monitor = self.monitor.lock().unwrap().take().unwrap();
        let renderer = self.renderer.lock().unwrap().take().unwrap();
        let capture = self.capture.lock().unwrap().take().unwrap();
        Ok((
            monitor.into(),
            renderer.into(),
            if self.state.invalid_renderer_id.load(Ordering::SeqCst) {
                0
            } else {
                13
            },
            capture.into(),
            if self.state.invalid_render_node.load(Ordering::SeqCst) {
                "renderD128".into()
            } else {
                "/dev/dri/renderD128".into()
            },
            if self.state.invalid_id.load(Ordering::SeqCst) {
                0
            } else {
                91
            },
        ))
    }

    async fn release_display_session(
        &self,
        id: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state.release_entered.notify_one();
        let gate = self.state.release_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.state
            .releases
            .lock()
            .unwrap()
            .push((id, header.destination().unwrap().to_string()));
        self.state.released.notify_one();
        if self.state.release_error.load(Ordering::SeqCst) {
            Err(zbus::fdo::Error::Failed("release rejected".into()))
        } else {
            Ok(())
        }
    }

    fn acquire_renderer(
        &self,
        session: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(BusFd, u64)> {
        assert_eq!(session, 91);
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert_eq!(header.destination().unwrap().as_str(), ":1.88");
        Ok((fd.into(), 14))
    }

    fn release_renderer(
        &self,
        session: u64,
        renderer: u64,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state.renderer_releases.lock().unwrap().push((
            session,
            renderer,
            header.destination().unwrap().to_string(),
        ));
        Ok(())
    }
}

struct Fixture {
    _server: zbus::Connection,
    monitor_peer: std::os::unix::net::UnixStream,
    renderer_peer: std::os::unix::net::UnixStream,
    capture_peer: std::os::unix::net::UnixStream,
    provider: Provider,
    state: Arc<State>,
}

impl Fixture {
    async fn new(stalled: bool) -> Self {
        let state = Arc::new(State {
            owner: Mutex::new(":1.88".into()),
            gate: Mutex::new(stalled.then(|| Arc::new(Notify::new()))),
            ..State::default()
        });
        let (monitor, monitor_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (renderer, renderer_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (capture, capture_peer) = std::os::unix::net::UnixStream::pair().unwrap();
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
            capture_peer,
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

#[tokio::test]
async fn session_carries_monitor_capture_and_a_bound_renderer() {
    let mut fixture = Fixture::new(false).await;
    let mut session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(session.target().device_major, 226);
    assert_eq!(session.target().device_minor, 42);
    assert_eq!(session.target().crtc_id.get(), 7);
    assert_eq!(session.target().connector_id.get(), 11);
    let (renderer, renderer_id, issuer) = session.take_renderer().unwrap().into_parts();
    assert_eq!(renderer_id.get(), 13);
    assert_eq!(
        issuer.render_node(),
        std::path::Path::new("/dev/dri/renderD128")
    );
    fixture.renderer_peer.write_all(&[0x26]).unwrap();
    let mut renderer = std::os::unix::net::UnixStream::from(renderer);
    let mut byte = [0];
    renderer.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0x26]);
    fixture.monitor_peer.write_all(&[0x37]).unwrap();
    let mut monitor =
        std::os::unix::net::UnixStream::from(session.monitor().try_clone_to_owned().unwrap());
    let mut byte = [0];
    monitor.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0x37]);
    fixture.capture_peer.write_all(&[0x49]).unwrap();
    let mut capture =
        std::os::unix::net::UnixStream::from(session.capture_access().unwrap().into_fd());
    capture.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [0x49]);
    let replacement = issuer.acquire().await.unwrap();
    let (_, replacement_id, issuer) = replacement.into_parts();
    assert_eq!(replacement_id.get(), 14);
    issuer.release(replacement_id).await.unwrap();
    assert_eq!(
        fixture.state.renderer_releases.lock().unwrap().as_slice(),
        &[(91, 14, ":1.88".into())]
    );
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    session.release().await.unwrap();
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
}

#[tokio::test]
async fn cancellation_releases_a_late_session() {
    let fixture = Fixture::new(true).await;
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let provider = fixture.provider.clone();
        let cancellation = cancellation.clone();
        async move { provider.acquire(target(), cancellation).await }
    });
    fixture.state.entered.notified().await;
    cancellation.cancel();
    assert!(matches!(task.await.unwrap(), Err(Error::Cancelled)));
    fixture
        .state
        .gate
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .notify_one();
    tokio::time::timeout(Duration::from_secs(2), fixture.state.released.notified())
        .await
        .unwrap();
    assert_eq!(fixture.state.releases.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropping_a_session_releases_the_issuing_owner() {
    let mut fixture = Fixture::new(false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    drop(session);
    tokio::time::timeout(Duration::from_secs(2), fixture.state.released.notified())
        .await
        .unwrap();
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
    for peer in [
        &mut fixture.monitor_peer,
        &mut fixture.renderer_peer,
        &mut fixture.capture_peer,
    ] {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[tokio::test]
async fn invalid_session_id_closes_capabilities() {
    let mut fixture = Fixture::new(false).await;
    fixture.state.invalid_id.store(true, Ordering::SeqCst);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::InvalidSession)
    ));
    for peer in [
        &mut fixture.monitor_peer,
        &mut fixture.renderer_peer,
        &mut fixture.capture_peer,
    ] {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[tokio::test]
async fn invalid_renderer_id_closes_capabilities() {
    let mut fixture = Fixture::new(false).await;
    fixture
        .state
        .invalid_renderer_id
        .store(true, Ordering::SeqCst);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::InvalidRenderer)
    ));
    for peer in [
        &mut fixture.monitor_peer,
        &mut fixture.renderer_peer,
        &mut fixture.capture_peer,
    ] {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[tokio::test]
async fn relative_render_node_closes_capabilities() {
    let mut fixture = Fixture::new(false).await;
    fixture
        .state
        .invalid_render_node
        .store(true, Ordering::SeqCst);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::InvalidRenderNode)
    ));
    for peer in [
        &mut fixture.monitor_peer,
        &mut fixture.renderer_peer,
        &mut fixture.capture_peer,
    ] {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[tokio::test]
async fn cancellation_before_acquisition_does_not_contact_mutter() {
    let fixture = Fixture::new(false).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        fixture.provider.acquire(target(), cancellation).await,
        Err(Error::Cancelled)
    ));
    assert_eq!(*fixture.state.requests.lock().unwrap(), 0);
    assert_eq!(fixture.provider.slots.available_permits(), 1);
}

#[tokio::test]
async fn abandoned_acquisition_drains_its_late_reply() {
    let fixture = Fixture::new(true).await;
    let task = {
        let provider = fixture.provider.clone();
        tokio::spawn(async move { provider.acquire(target(), CancellationToken::new()).await })
    };
    fixture.state.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.provider.slots.available_permits(), 0);
    fixture
        .state
        .gate
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .notify_one();
    tokio::time::timeout(Duration::from_secs(2), fixture.state.released.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn timed_out_acquisition_keeps_its_capacity_until_cleanup() {
    let mut fixture = Fixture::new(true).await;
    fixture.provider.timeout = Duration::from_millis(20);
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
    assert_eq!(*fixture.state.requests.lock().unwrap(), 1);
    fixture
        .state
        .gate
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .notify_one();
    tokio::time::timeout(Duration::from_secs(2), fixture.state.released.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn explicit_release_reports_a_broker_error() {
    let fixture = Fixture::new(false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    fixture.state.release_error.store(true, Ordering::SeqCst);
    assert!(matches!(session.release().await, Err(Error::Bus(_))));
}

#[tokio::test]
async fn stalled_release_does_not_free_provider_capacity() {
    let mut fixture = Fixture::new(false).await;
    let mut session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    session.timeout = Duration::from_millis(20);
    fixture.provider.timeout = Duration::from_millis(20);
    let gate = Arc::new(Notify::new());
    *fixture.state.release_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let release = tokio::spawn(session.release());
    fixture.state.release_entered.notified().await;
    assert!(matches!(release.await.unwrap(), Err(Error::Timeout)));
    assert_eq!(fixture.provider.slots.available_permits(), 0);
    assert!(matches!(
        fixture
            .provider
            .acquire(target(), CancellationToken::new())
            .await,
        Err(Error::Timeout)
    ));
    assert_eq!(*fixture.state.requests.lock().unwrap(), 1);
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(2), fixture.state.released.notified())
        .await
        .unwrap();
}
