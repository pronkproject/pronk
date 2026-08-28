use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pronk_backend_protocol::{
    session_object_path, validate_error_text, BackendInfo, DeviceIdentity, DeviceInfo,
    DeviceSnapshot, SessionOptions, Validate,
};
use thiserror::Error;
use tokio::sync::{mpsc, watch, Mutex};
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::device::{DeviceActor, DeviceActorError, DeviceConnector};
use crate::discovery::{DiscoveryActorError, DiscoveryEvent, DiscoveryHandle};
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
    active_session: Mutex<Option<ActiveSession>>,
    shutdown: watch::Sender<bool>,
}

#[derive(Debug)]
struct ActiveSession {
    session_id: String,
    object_path: OwnedObjectPath,
    actor: DeviceActor,
}

impl ChromiacastBackend {
    pub fn new(
        info: BackendInfo,
        discovery: DiscoveryHandle,
        connector: Arc<dyn DeviceConnector>,
        shutdown: watch::Sender<bool>,
    ) -> Self {
        Self {
            shared: Arc::new(BackendShared {
                info,
                connection_generation: AtomicU64::new(0),
                discovery,
                connector,
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

    pub(crate) async fn stop_session(&self, session_id: &str) -> Result<(), SessionLifecycleError> {
        let mut active = self.shared.active_session.lock().await;
        let Some(session) = active.as_ref() else {
            return Ok(());
        };
        if session.session_id != session_id {
            return Err(SessionLifecycleError::StaleSession);
        }
        let session = active
            .take()
            .expect("active session disappeared while locked");
        session.actor.shutdown().await?;
        Ok(())
    }

    pub(crate) async fn shutdown_active_session(&self) -> Result<(), SessionLifecycleError> {
        let mut active = self.shared.active_session.lock().await;
        let Some(session) = active.take() else {
            return Ok(());
        };
        session.actor.shutdown().await?;
        Ok(())
    }

    async fn finish_event_forwarder(&self, session_id: &str, object_path: &OwnedObjectPath) {
        let session = {
            let mut active = self.shared.active_session.lock().await;
            let matches = active.as_ref().is_some_and(|session| {
                session.session_id == session_id && session.object_path == *object_path
            });
            matches.then(|| active.take()).flatten()
        };
        let Some(session) = session else {
            return;
        };
        if let Err(error) = session.actor.shutdown().await {
            tracing::warn!(%error, %session_id, "failed to clean up Chromiacast session after its event forwarder exited");
        }
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
        #[zbus(object_server)] object_server: &ObjectServer,
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
        let mut active = self.shared.active_session.lock().await;
        if active.is_some() {
            return Err(zbus::fdo::Error::Failed(
                "Chromiacast supports one active session".into(),
            ));
        }
        let (actor, actor_handle, device_events) = DeviceActor::spawn(
            device,
            session_id.clone(),
            options.session_generation,
            options.requested_features,
            Arc::clone(&self.shared.connector),
        )
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let session =
            ChromiacastSession::new(self.clone(), session_id.clone(), path.clone(), actor_handle);
        if let Err(error) = object_server.at(path.clone(), session).await {
            let _ = actor.shutdown().await;
            return Err(zbus::fdo::Error::Failed(error.to_string()));
        }
        // Publish ownership only after the object exists. If this method is
        // cancelled while awaiting registration, the still-local actor drops
        // fail-closed instead of becoming an unreachable active session.
        *active = Some(ActiveSession {
            session_id: session_id.clone(),
            object_path: path.clone(),
            actor,
        });
        let event_connection = connection.clone();
        let cleanup_connection = connection.clone();
        let event_path = path.clone();
        let cleanup_path = path.clone();
        let event_backend = self.clone();
        let event_session_id = session_id.clone();
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
}

fn discovery_error(error: DiscoveryActorError) -> zbus::fdo::Error {
    match error {
        DiscoveryActorError::StaleGeneration { .. } => {
            zbus::fdo::Error::InvalidArgs(error.to_string())
        }
        _ => zbus::fdo::Error::Failed(error.to_string()),
    }
}
