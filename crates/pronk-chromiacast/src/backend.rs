use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pronk_backend_protocol::{
    session_object_path, validate_error_text, BackendInfo, DeviceIdentity, DeviceInfo,
    DeviceSnapshot, SessionOptions, Validate,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::device::{DeviceActor, DeviceActorError, DeviceConnector, DeviceEventReceivers};
use crate::discovery::{DeviceRecord, DiscoveryActorError, DiscoveryEvent, DiscoveryHandle};
use crate::media::VideoEncoderPolicy;
use crate::session::ChromiacastSession;

#[derive(Debug, Clone)]
pub struct ChromiacastBackend {
    shared: Arc<BackendShared>,
}

#[derive(Debug)]
struct BackendShared {
    info: BackendInfo,
    connection_generation: AtomicU64,
    discovery: DiscoveryHandle,
    connector: Arc<dyn DeviceConnector>,
    encoder_policy: VideoEncoderPolicy,
    active_session: Mutex<Option<ActiveSession>>,
    shutdown: watch::Sender<bool>,
}

#[derive(Debug)]
struct ActiveSession {
    session_id: String,
    object_path: OwnedObjectPath,
    actor: DeviceActor,
}

struct PendingSessionRegistration {
    connection: Connection,
    session_id: String,
    device: DeviceRecord,
    options: SessionOptions,
    path: OwnedObjectPath,
    reply: oneshot::Sender<zbus::fdo::Result<OwnedObjectPath>>,
    acknowledged: oneshot::Receiver<()>,
}

enum SessionSelection {
    Any,
    Exact {
        session_id: String,
        object_path: OwnedObjectPath,
    },
    IfCurrent {
        session_id: String,
        object_path: OwnedObjectPath,
    },
}

impl SessionSelection {
    fn accepts(
        &self,
        session_id: &str,
        object_path: &OwnedObjectPath,
    ) -> Result<bool, SessionLifecycleError> {
        match self {
            Self::Any => Ok(true),
            Self::Exact {
                session_id: expected_id,
                object_path: expected_path,
            } => {
                if expected_id == session_id && expected_path == object_path {
                    Ok(true)
                } else {
                    Err(SessionLifecycleError::StaleSession)
                }
            }
            Self::IfCurrent {
                session_id: expected_id,
                object_path: expected_path,
            } => Ok(expected_id == session_id && expected_path == object_path),
        }
    }
}

impl ChromiacastBackend {
    pub fn new(
        info: BackendInfo,
        discovery: DiscoveryHandle,
        connector: Arc<dyn DeviceConnector>,
        encoder_policy: VideoEncoderPolicy,
        shutdown: watch::Sender<bool>,
    ) -> Self {
        Self {
            shared: Arc::new(BackendShared {
                info,
                connection_generation: AtomicU64::new(0),
                discovery,
                connector,
                encoder_policy,
                active_session: Mutex::new(None),
                shutdown,
            }),
        }
    }

    pub fn complete_registration(&self, connection_generation: u64) {
        debug_assert_ne!(connection_generation, 0);
        self.shared
            .connection_generation
            .store(connection_generation, Ordering::Release);
    }

    fn connection_generation(&self) -> zbus::fdo::Result<u64> {
        match self.shared.connection_generation.load(Ordering::Acquire) {
            0 => Err(zbus::fdo::Error::Failed(
                "backend registration is incomplete".into(),
            )),
            generation => Ok(generation),
        }
    }

    pub(crate) async fn stop_session(
        &self,
        session_id: &str,
        object_path: &OwnedObjectPath,
    ) -> Result<(), SessionLifecycleError> {
        self.stop_owned(SessionSelection::Exact {
            session_id: session_id.into(),
            object_path: object_path.clone(),
        })
        .await
    }

    pub(crate) async fn shutdown_active_session(&self) -> Result<(), SessionLifecycleError> {
        self.stop_owned(SessionSelection::Any).await
    }

    async fn finish_event_forwarder(&self, session_id: &str, object_path: &OwnedObjectPath) {
        if let Err(error) = self
            .stop_owned(SessionSelection::IfCurrent {
                session_id: session_id.into(),
                object_path: object_path.clone(),
            })
            .await
        {
            tracing::warn!(%error, %session_id, "failed to clean up Chromiacast session after its event forwarder exited");
        }
    }

    async fn stop_owned(&self, selection: SessionSelection) -> Result<(), SessionLifecycleError> {
        let backend = self.clone();
        tokio::spawn(async move {
            let mut active = backend.shared.active_session.lock().await;
            let Some(session) = active.as_ref() else {
                return Ok(());
            };
            if !selection.accepts(&session.session_id, &session.object_path)? {
                return Ok(());
            }
            let session = active
                .take()
                .expect("active session disappeared while locked");
            // Retain the slot lock until this actor finishes final cleanup.
            // A canceled D-Bus waiter only drops its JoinHandle.
            let session_id = session.session_id;
            let result = session
                .actor
                .shutdown()
                .await
                .map_err(SessionLifecycleError::from);
            if let Err(error) = &result {
                tracing::warn!(%error, %session_id, "Chromiacast session cleanup failed");
            }
            result
        })
        .await
        .map_err(|error| SessionLifecycleError::Join(error.to_string()))?
    }

    async fn register_session(&self, registration: PendingSessionRegistration) {
        let PendingSessionRegistration {
            connection,
            session_id,
            device,
            options,
            path,
            reply,
            acknowledged,
        } = registration;
        let mut active = self.shared.active_session.lock().await;
        if active.is_some() {
            let _ = reply.send(Err(zbus::fdo::Error::Failed(
                "Chromiacast supports one active session".into(),
            )));
            return;
        }
        let (actor, actor_handle, device_events) = match DeviceActor::spawn(
            device,
            session_id.clone(),
            options.session_generation,
            options.requested_features,
            Arc::clone(&self.shared.connector),
            self.shared.encoder_policy.clone(),
        ) {
            Ok(spawned) => spawned,
            Err(error) => {
                let _ = reply.send(Err(zbus::fdo::Error::Failed(error.to_string())));
                return;
            }
        };
        let session =
            ChromiacastSession::new(self.clone(), session_id.clone(), path.clone(), actor_handle);
        match connection.object_server().at(path.clone(), session).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = actor.shutdown().await;
                let _ = reply.send(Err(zbus::fdo::Error::Failed(
                    "Chromiacast session object path is already registered".into(),
                )));
                return;
            }
            Err(error) => {
                if let Err(cleanup) = connection
                    .object_server()
                    .remove::<ChromiacastSession, _>(&path)
                    .await
                {
                    tracing::warn!(%cleanup, %session_id, "failed to remove rejected Chromiacast session object");
                }
                let _ = actor.shutdown().await;
                let _ = reply.send(Err(zbus::fdo::Error::Failed(error.to_string())));
                return;
            }
        }
        // Registration may finish after the D-Bus request has been cancelled.
        // Keep the slot locked until the waiter accepts the path, then publish
        // ownership before another request can observe the registered object.
        if reply.send(Ok(path.clone())).is_err() || acknowledged.await.is_err() {
            if let Err(error) = connection
                .object_server()
                .remove::<ChromiacastSession, _>(&path)
                .await
            {
                tracing::warn!(%error, %session_id, "failed to remove abandoned Chromiacast session object");
            }
            let _ = actor.shutdown().await;
            return;
        }
        *active = Some(ActiveSession {
            session_id: session_id.clone(),
            object_path: path.clone(),
            actor,
        });
        drop(active);
        self.spawn_session_forwarder(connection, session_id, path, device_events);
    }

    fn spawn_session_forwarder(
        &self,
        connection: Connection,
        session_id: String,
        path: OwnedObjectPath,
        device_events: DeviceEventReceivers,
    ) {
        let event_connection = connection.clone();
        let cleanup_connection = connection.clone();
        let event_path = path.clone();
        let cleanup_path = path;
        let event_backend = self.clone();
        let event_session_id = session_id;
        connection
            .executor()
            .spawn(
                async move {
                    let result = crate::session::forward_device_events(
                        event_connection,
                        event_path,
                        device_events,
                    )
                    .await;
                    if let Err(error) = result {
                        tracing::warn!(%error, session_id = %event_session_id, "Chromiacast session event forwarder failed");
                    }
                    event_backend
                        .finish_event_forwarder(&event_session_id, &cleanup_path)
                        .await;
                    if let Err(error) = cleanup_connection
                        .object_server()
                        .remove::<ChromiacastSession, _>(&cleanup_path)
                        .await
                    {
                        tracing::warn!(%error, session_id = %event_session_id, "failed to remove stopped Chromiacast session object");
                    }
                },
                "forward Chromiacast media feedback",
            )
            .detach();
    }

    pub async fn forward_discovery_events(
        &self,
        connection: Connection,
        mut events: mpsc::Receiver<DiscoveryEvent>,
    ) -> zbus::Result<()> {
        let emitter = SignalEmitter::new(&connection, pronk_backend_protocol::BACKEND_PATH)?;
        while let Some(event) = events.recv().await {
            match event {
                DiscoveryEvent::Added {
                    discovery_generation,
                    revision,
                    device,
                } => Self::device_added(&emitter, discovery_generation, revision, device).await?,
                DiscoveryEvent::Changed {
                    discovery_generation,
                    revision,
                    device,
                } => Self::device_changed(&emitter, discovery_generation, revision, device).await?,
                DiscoveryEvent::Removed {
                    discovery_generation,
                    revision,
                    device,
                } => Self::device_removed(&emitter, discovery_generation, revision, device).await?,
                DiscoveryEvent::Fatal { error_text } => {
                    let error_text = if validate_error_text(&error_text).is_ok() {
                        error_text
                    } else {
                        "Chromiacast discovery failed".into()
                    };
                    Self::fatal_error(&emitter, self.connection_generation()?, error_text).await?;
                    break;
                }
            }
        }
        Ok(())
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk.Backend1")]
impl ChromiacastBackend {
    fn get_info(&self) -> BackendInfo {
        self.shared.info.clone()
    }

    async fn start_discovery(&self) -> zbus::fdo::Result<u64> {
        self.connection_generation()?;
        self.shared.discovery.start().await.map_err(discovery_error)
    }

    async fn stop_discovery(&self, discovery_generation: u64) -> zbus::fdo::Result<()> {
        self.connection_generation()?;
        self.shared
            .discovery
            .stop(discovery_generation)
            .await
            .map_err(discovery_error)
    }

    async fn list_devices(&self) -> zbus::fdo::Result<DeviceSnapshot> {
        self.connection_generation()?;
        self.shared
            .discovery
            .snapshot()
            .await
            .map_err(discovery_error)
    }

    async fn create_session(
        &self,
        session_id: String,
        device_id: String,
        options: SessionOptions,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        options
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        let path = session_object_path(&session_id, options.session_generation)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        let connection_generation = self.connection_generation()?;
        if options.connection_generation != connection_generation {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "stale connection generation {}; active generation is {connection_generation}",
                options.connection_generation
            )));
        }
        let device = self
            .shared
            .discovery
            .resolve(options.discovery_generation, device_id)
            .await
            .map_err(discovery_error)?
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs("device is not in the active inventory".into())
            })?;
        let (reply, response) = oneshot::channel();
        let (acknowledge, acknowledged) = oneshot::channel();
        let backend = self.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            backend
                .register_session(PendingSessionRegistration {
                    connection,
                    session_id,
                    device,
                    options,
                    path,
                    reply,
                    acknowledged,
                })
                .await;
        });
        let path = response
            .await
            .map_err(|_| zbus::fdo::Error::Failed("session registration task stopped".into()))??;
        let _ = acknowledge.send(());
        Ok(path)
    }

    fn shutdown(&self) {
        self.shared.shutdown.send_replace(true);
    }

    #[zbus(signal)]
    async fn device_added(
        emitter: &SignalEmitter<'_>,
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_changed(
        emitter: &SignalEmitter<'_>,
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_removed(
        emitter: &SignalEmitter<'_>,
        discovery_generation: u64,
        revision: u64,
        device: DeviceIdentity,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn fatal_error(
        emitter: &SignalEmitter<'_>,
        connection_generation: u64,
        error_text: String,
    ) -> zbus::Result<()>;
}

#[derive(Debug, Error)]
pub(crate) enum SessionLifecycleError {
    #[error("session object is stale")]
    StaleSession,
    #[error(transparent)]
    Device(#[from] DeviceActorError),
    #[error("join backend session cleanup: {0}")]
    Join(String),
}

fn discovery_error(error: DiscoveryActorError) -> zbus::fdo::Error {
    match error {
        DiscoveryActorError::StaleGeneration { .. } => {
            zbus::fdo::Error::InvalidArgs(error.to_string())
        }
        _ => zbus::fdo::Error::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::ChromiacastDeviceConnector;
    use crate::discovery::{DiscoveryActor, DiscoveryConfiguration, EmptyTestDiscoverySource};
    use pronk_backend_protocol::DeviceAvailability;
    use tokio::net::UnixStream;
    use zbus::connection::{AuthMechanism, Builder};
    use zbus::Guid;

    struct ExistingSession;

    #[zbus::interface(name = "io.github.pronkproject.Pronk.BackendSession1")]
    impl ExistingSession {
        fn ping(&self) -> bool {
            true
        }
    }

    #[test]
    fn stale_session_object_cannot_stop_a_replacement_with_the_same_id() {
        let old_path = OwnedObjectPath::try_from("/session/old").unwrap();
        let new_path = OwnedObjectPath::try_from("/session/new").unwrap();
        let exact = SessionSelection::Exact {
            session_id: "display".into(),
            object_path: old_path.clone(),
        };
        assert!(exact.accepts("display", &old_path).unwrap());
        assert!(matches!(
            exact.accepts("display", &new_path),
            Err(SessionLifecycleError::StaleSession)
        ));
        assert!(!SessionSelection::IfCurrent {
            session_id: "display".into(),
            object_path: old_path,
        }
        .accepts("display", &new_path)
        .unwrap());
    }

    #[tokio::test]
    async fn registration_cleans_abandoned_sessions_and_preserves_existing_objects() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = Builder::unix_stream(server_stream)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let client = Builder::unix_stream(client_stream)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (connection, _client) = tokio::try_join!(server.build(), client.build()).unwrap();
        let (discovery, handle, _) = DiscoveryActor::spawn(
            Box::new(EmptyTestDiscoverySource),
            DiscoveryConfiguration::default(),
        );
        let (shutdown, _) = watch::channel(false);
        let backend = ChromiacastBackend::new(
            BackendInfo::new("chromiacast", "test", "test", "test", "test"),
            handle,
            Arc::new(ChromiacastDeviceConnector),
            VideoEncoderPolicy::Software,
            shutdown,
        );
        let path = OwnedObjectPath::try_from("/session/abandoned").unwrap();
        let device = DeviceRecord {
            info: DeviceInfo {
                backend_id: "chromiacast".into(),
                device_id: "test-device".into(),
                display_name: "Test device".into(),
                availability: DeviceAvailability::Available,
                metadata: Vec::new(),
            },
            endpoints: Vec::new(),
        };
        let (reply, response) = oneshot::channel();
        let (acknowledge, acknowledged) = oneshot::channel();
        drop(response);
        drop(acknowledge);

        backend
            .register_session(PendingSessionRegistration {
                connection: connection.clone(),
                session_id: "test-session".into(),
                device: device.clone(),
                options: SessionOptions {
                    connection_generation: 1,
                    discovery_generation: 1,
                    session_generation: 1,
                    requested_features: 0,
                },
                path: path.clone(),
                reply,
                acknowledged,
            })
            .await;
        assert!(backend.shared.active_session.lock().await.is_none());
        assert!(connection
            .object_server()
            .interface::<_, ChromiacastSession>(&path)
            .await
            .is_err());

        let acknowledged_path = OwnedObjectPath::try_from("/session/unacknowledged").unwrap();
        let (reply, response) = oneshot::channel();
        let (acknowledge, acknowledged) = oneshot::channel();
        drop(acknowledge);
        backend
            .register_session(PendingSessionRegistration {
                connection: connection.clone(),
                session_id: "test-session".into(),
                device: device.clone(),
                options: SessionOptions {
                    connection_generation: 1,
                    discovery_generation: 1,
                    session_generation: 2,
                    requested_features: 0,
                },
                path: acknowledged_path.clone(),
                reply,
                acknowledged,
            })
            .await;
        assert_eq!(response.await.unwrap().unwrap(), acknowledged_path);
        assert!(backend.shared.active_session.lock().await.is_none());
        assert!(connection
            .object_server()
            .interface::<_, ChromiacastSession>(&acknowledged_path)
            .await
            .is_err());

        let existing_path = OwnedObjectPath::try_from("/session/existing").unwrap();
        assert!(connection
            .object_server()
            .at(existing_path.clone(), ExistingSession)
            .await
            .unwrap());
        let (reply, response) = oneshot::channel();
        let (_acknowledge, acknowledged) = oneshot::channel();
        backend
            .register_session(PendingSessionRegistration {
                connection: connection.clone(),
                session_id: "test-session".into(),
                device: device.clone(),
                options: SessionOptions {
                    connection_generation: 1,
                    discovery_generation: 1,
                    session_generation: 3,
                    requested_features: 0,
                },
                path: existing_path.clone(),
                reply,
                acknowledged,
            })
            .await;
        assert!(response.await.unwrap().is_err());
        assert!(backend.shared.active_session.lock().await.is_none());
        assert!(connection
            .object_server()
            .interface::<_, ExistingSession>(&existing_path)
            .await
            .is_ok());

        let accepted_path = OwnedObjectPath::try_from("/session/accepted").unwrap();
        let (reply, response) = oneshot::channel();
        let (acknowledge, acknowledged) = oneshot::channel();
        let registering = {
            let backend = backend.clone();
            let connection = connection.clone();
            let accepted_path = accepted_path.clone();
            tokio::spawn(async move {
                backend
                    .register_session(PendingSessionRegistration {
                        connection,
                        session_id: "test-session".into(),
                        device,
                        options: SessionOptions {
                            connection_generation: 1,
                            discovery_generation: 1,
                            session_generation: 4,
                            requested_features: 0,
                        },
                        path: accepted_path,
                        reply,
                        acknowledged,
                    })
                    .await;
            })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), response)
                .await
                .expect("registration did not return the accepted object path")
                .unwrap()
                .unwrap(),
            accepted_path
        );
        acknowledge.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), registering)
            .await
            .expect("registration did not publish after acknowledgement")
            .unwrap();
        assert!(backend.shared.active_session.lock().await.is_some());
        assert!(connection
            .object_server()
            .interface::<_, ChromiacastSession>(&accepted_path)
            .await
            .is_ok());
        backend.shutdown_active_session().await.unwrap();
        discovery.shutdown().await.unwrap();
    }
}
