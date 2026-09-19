//! Exercise the application adapter over a private, in-process bus connection.

use super::*;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use zbus::connection::{AuthMechanism, Builder};
use zbus::message::Header;
use zbus::zvariant::OwnedFd as BusFd;

const SERVICE: &str = "org.gnome.Mutter.CastKms";
const PATH: &str = "/org/gnome/Mutter/CastKms";

#[derive(Debug, PartialEq)]
enum Event {
    Acquire(String),
    ReleaseDisplay(String),
}

struct State {
    events: Mutex<Vec<Event>>,
    owner: Mutex<String>,
    entered: Notify,
    gate: Option<Notify>,
    changed: Notify,
}

struct Bus(Arc<State>);

#[zbus::interface(name = "org.freedesktop.DBus")]
impl Bus {
    fn get_name_owner(&self, name: &str) -> String {
        assert_eq!(name, SERVICE);
        self.0.owner.lock().unwrap().clone()
    }
}

struct Broker(Arc<State>);

fn destination(header: Header<'_>) -> String {
    header.destination().unwrap().to_string()
}

#[zbus::interface(name = "org.gnome.Mutter.CastKms")]
impl Broker {
    async fn create_display_session(
        &self,
        major: u32,
        minor: u32,
        crtc: u32,
        connector: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> (BusFd, BusFd, u64) {
        assert_eq!((major, minor, crtc, connector), (226, 9, 17, 29));
        self.0
            .events
            .lock()
            .unwrap()
            .push(Event::Acquire(destination(header)));
        self.0.entered.notify_one();
        if let Some(gate) = &self.0.gate {
            gate.notified().await;
        }
        let fd = || -> BusFd {
            let file: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
            file.into()
        };
        (fd(), fd(), 7)
    }

    fn release_display_session(&self, session: u64, #[zbus(header)] header: Header<'_>) {
        assert_eq!(session, 7);
        self.0
            .events
            .lock()
            .unwrap()
            .push(Event::ReleaseDisplay(destination(header)));
        self.0.changed.notify_one();
    }
}

struct Fixture {
    server: zbus::Connection,
    provider: Provider,
    state: Arc<State>,
}

impl Fixture {
    async fn new(stall: bool) -> Self {
        let state = Arc::new(State {
            events: Mutex::new(Vec::new()),
            owner: Mutex::new(":1.37".into()),
            entered: Notify::new(),
            gate: stall.then(Notify::new),
            changed: Notify::new(),
        });
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let server = Builder::unix_stream(server)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at("/org/freedesktop/DBus", Bus(Arc::clone(&state)))
            .unwrap()
            .serve_at(PATH, Broker(Arc::clone(&state)))
            .unwrap();
        let client = Builder::unix_stream(client)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (server, client) = tokio::try_join!(server.build(), client.build()).unwrap();
        Self {
            server,
            provider: Provider::new(
                client,
                NonZeroUsize::new(1).unwrap(),
                Duration::from_secs(2),
            )
            .unwrap(),
            state,
        }
    }

    async fn acquire(&self) -> KernelSession {
        KernelSessionProvider::acquire(
            &self.provider,
            &super::tests::output(),
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    async fn events_reach(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.state.changed.notified();
                if self.state.events.lock().unwrap().len() >= count {
                    break;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn unused_authority_is_released_in_order_to_the_original_issuer() {
    let fixture = Fixture::new(false).await;
    let session = fixture.acquire().await;
    *fixture.state.owner.lock().unwrap() = ":1.99".into();
    session.release().await.unwrap();
    assert_eq!(
        fixture.state.events.lock().unwrap().as_slice(),
        &[
            Event::Acquire(":1.37".into()),
            Event::ReleaseDisplay(":1.37".into()),
        ]
    );
}

#[tokio::test]
async fn broker_does_not_supply_renderer_authority() {
    let fixture = Fixture::new(false).await;
    let mut session = fixture.acquire().await;
    assert_eq!(
        session.take_renderer_access().unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    session.release().await.unwrap();
    assert_eq!(
        fixture.state.events.lock().unwrap().as_slice(),
        &[
            Event::Acquire(":1.37".into()),
            Event::ReleaseDisplay(":1.37".into()),
        ]
    );
}

#[tokio::test]
async fn cancellation_drains_a_late_session_without_publishing_it() {
    let fixture = Fixture::new(true).await;
    let cancellation = CancellationToken::new();
    let provider = fixture.provider.clone();
    let cancelled = cancellation.clone();
    let task = tokio::spawn(async move {
        KernelSessionProvider::acquire(&provider, &super::tests::output(), false, cancelled).await
    });
    fixture.state.entered.notified().await;
    cancellation.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(KernelSessionError::Cancelled)
    ));
    fixture.state.gate.as_ref().unwrap().notify_one();
    fixture.events_reach(2).await;
    assert_eq!(
        fixture.state.events.lock().unwrap().as_slice(),
        &[
            Event::Acquire(":1.37".into()),
            Event::ReleaseDisplay(":1.37".into()),
        ]
    );
}

#[tokio::test]
async fn losing_the_issuer_reports_release_failure_without_hanging() {
    let fixture = Fixture::new(false).await;
    let session = fixture.acquire().await;
    fixture.server.close().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), session.release())
            .await
            .unwrap()
            .is_err()
    );
}
