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
    owner: Mutex<String>,
    entered: Notify,
    released: Notify,
    gate: Option<Notify>,
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
    capture: Mutex<Option<OwnedFd>>,
}

#[zbus::interface(name = "org.gnome.Mutter.CastKms")]
impl Mutter {
    async fn create_capture_grant(
        &self,
        major: u32,
        minor: u32,
        crtc: u32,
        connector: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(BusFd, u64)> {
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
        let capture = self
            .capture
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| zbus::fdo::Error::Failed("capture already issued".into()))?;
        Ok((capture.into(), 91))
    }

    fn release_capture_grant(
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

#[tokio::test]
async fn release_uses_the_issuing_owner_even_after_service_replacement() {
    let mut fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    fixture._peer.write_all(&[0x49]).unwrap();
    let mut capture =
        std::os::unix::net::UnixStream::from(session.as_fd().try_clone_to_owned().unwrap());
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
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    drop(session);
    notified(&fixture.state.released).await;
    assert_eq!(
        fixture.state.releases.lock().unwrap().as_slice(),
        &[(91, ":1.88".into())]
    );
}

#[tokio::test]
async fn dropping_a_session_requests_release() {
    let fixture = Fixture::new(false, false).await;
    let session = fixture
        .provider
        .acquire(target(), CancellationToken::new())
        .await
        .unwrap();
    drop(session);
    notified(&fixture.state.released).await;
    assert_eq!(fixture.state.releases.lock().unwrap().len(), 1);
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
