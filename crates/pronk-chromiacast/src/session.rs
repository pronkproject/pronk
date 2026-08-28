use pronk_backend_protocol::{
    validate_media_configuration, ControlOperation, DeviceCapabilities, MediaConfiguration,
    PipeWireTarget, PreparationRequest, SessionStatistics, StopReason, SuspendReason, Validate,
};
use zbus::object_server::{ResponseDispatchNotifier, SignalEmitter};
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use zbus::Connection;

use crate::backend::{ChromiacastBackend, SessionLifecycleError};
use crate::device::{
    DeviceActorError, DeviceActorHandle, DeviceBitrateRequest, DeviceEvent, DeviceEventReceivers,
};
use crate::media::MediaSessionError;

#[derive(Debug, Clone)]
pub(crate) struct ChromiacastSession {
    backend: ChromiacastBackend,
    session_id: String,
    object_path: OwnedObjectPath,
    actor: DeviceActorHandle,
}

impl ChromiacastSession {
    pub(crate) fn new(
        backend: ChromiacastBackend,
        session_id: String,
        object_path: OwnedObjectPath,
        actor: DeviceActorHandle,
    ) -> Self {
        Self {
            backend,
            session_id,
            object_path,
            actor,
        }
    }
}

pub(crate) async fn forward_device_events(
    connection: Connection,
    object_path: OwnedObjectPath,
    receivers: DeviceEventReceivers,
) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(&connection, object_path)?;
    let mut events = receivers.events;
    let mut bitrate_requests = receivers.bitrate_requests;
    let mut fatal_error = receivers.fatal_error;
    let mut bitrate_requests_open = true;
    loop {
        tokio::select! {
            biased;
            fatal_error = &mut fatal_error => {
                let Ok(fatal_error) = fatal_error else {
                    break;
                };
                ChromiacastSession::fatal_error(
                    &emitter,
                    fatal_error.session_generation,
                    fatal_error.error_text,
                )
                .await?;
                break;
            }
            changed = bitrate_requests.changed(), if bitrate_requests_open => {
                if changed.is_err() {
                    bitrate_requests_open = false;
                    continue;
                }
                let request = bitrate_requests.borrow_and_update().clone();
                if let Some(request) = request {
                    forward_bitrate_request(&emitter, request).await?;
                }
            }
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                forward_device_event(&emitter, event).await?;
            }
        }
    }
    Ok(())
}

async fn forward_bitrate_request(
    emitter: &SignalEmitter<'_>,
    request: DeviceBitrateRequest,
) -> zbus::Result<()> {
    ChromiacastSession::bitrate_requested(
        emitter,
        request.session_generation,
        request.media_generation,
        request.bitrate,
    )
    .await
}

async fn forward_device_event(emitter: &SignalEmitter<'_>, event: DeviceEvent) -> zbus::Result<()> {
    match event {
        DeviceEvent::KeyFrameRequested {
            session_generation,
            media_generation,
        } => {
            ChromiacastSession::keyframe_requested(emitter, session_generation, media_generation)
                .await
        }
        DeviceEvent::ControlCompleted {
            session_generation,
            operation_id,
            succeeded,
            error_text,
        } => {
            ChromiacastSession::control_completed(
                emitter,
                session_generation,
                operation_id,
                succeeded,
                error_text,
            )
            .await
        }
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk.BackendSession1")]
impl ChromiacastSession {
    async fn prepare(&self, request: PreparationRequest) -> zbus::fdo::Result<DeviceCapabilities> {
        request
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        self.actor.prepare(request).await.map_err(device_error)
    }

    async fn configure_media(
        &self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: u64,
    ) -> zbus::fdo::Result<()> {
        validate_media_configuration(remotes.len(), &targets, &configuration, media_generation)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        self.actor
            .configure_media(remotes, targets, configuration, media_generation)
            .await
            .map_err(device_error)
    }

    async fn start(&self, media_generation: u64) -> zbus::fdo::Result<()> {
        require_generation("media", media_generation)?;
        self.actor
            .start_media(media_generation)
            .await
            .map_err(device_error)
    }

    async fn suspend(&self, reason: SuspendReason) -> zbus::fdo::Result<()> {
        self.actor.suspend_media(reason).await.map_err(device_error)
    }

    async fn resume(&self, media_generation: u64) -> zbus::fdo::Result<()> {
        require_generation("media", media_generation)?;
        self.actor
            .resume_media(media_generation)
            .await
            .map_err(device_error)
    }

    async fn stop_media(&self, media_generation: u64, reason: StopReason) -> zbus::fdo::Result<()> {
        require_generation("media", media_generation)?;
        self.actor
            .stop_media(media_generation, reason)
            .await
            .map_err(device_error)
    }

    async fn stop(
        &self,
        _reason: StopReason,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<ResponseDispatchNotifier<()>> {
        self.backend
            .stop_session(&self.session_id)
            .await
            .map_err(session_lifecycle_error)?;
        let (reply, dispatched) = ResponseDispatchNotifier::new(());
        let connection = connection.clone();
        let removal_connection = connection.clone();
        let object_path = self.object_path.clone();
        connection
            .executor()
            .spawn(
                async move {
                    dispatched.await;
                    let _ = removal_connection
                        .object_server()
                        .remove::<ChromiacastSession, _>(object_path)
                        .await;
                },
                "remove stopped Chromiacast session",
            )
            .detach();
        Ok(reply)
    }

    async fn transmit_control(&self, operation: ControlOperation) -> zbus::fdo::Result<u64> {
        operation
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        self.actor
            .transmit_control(operation)
            .await
            .map_err(device_error)
    }

    async fn get_statistics(&self) -> zbus::fdo::Result<SessionStatistics> {
        self.actor.statistics().await.map_err(device_error)
    }

    #[zbus(signal)]
    async fn state_changed(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        media_generation: u64,
        state: pronk_backend_protocol::SessionState,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn disconnected(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        error_text: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn keyframe_requested(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        media_generation: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn bitrate_requested(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        media_generation: u64,
        bitrate: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn control_completed(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        operation_id: u64,
        succeeded: bool,
        error_text: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn fatal_error(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        error_text: String,
    ) -> zbus::Result<()>;
}

fn require_generation(name: &'static str, generation: u64) -> zbus::fdo::Result<()> {
    if generation == 0 {
        return Err(zbus::fdo::Error::InvalidArgs(format!(
            "{name} generation must be nonzero"
        )));
    }
    Ok(())
}

fn device_error(error: DeviceActorError) -> zbus::fdo::Error {
    match error {
        DeviceActorError::InvalidRequest(_)
        | DeviceActorError::AlreadyPrepared
        | DeviceActorError::DeviceIdentityChanged
        | DeviceActorError::Media(MediaSessionError::InvalidRequest(_)) => {
            zbus::fdo::Error::InvalidArgs(error.to_string())
        }
        _ => zbus::fdo::Error::Failed(error.to_string()),
    }
}

fn session_lifecycle_error(error: SessionLifecycleError) -> zbus::fdo::Error {
    match error {
        SessionLifecycleError::StaleSession => zbus::fdo::Error::InvalidArgs(error.to_string()),
        SessionLifecycleError::Device(error) => device_error(error),
    }
}
