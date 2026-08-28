//! Public session-bus adapter for Device inventory and cast-display setup.

use pronk_dbus::{
    ApiVersion, CastDisplayInfo, CastDisplaySnapshot, CastDisplayState, DeviceInfo,
    DeviceSelection, DeviceSnapshot, DisplayAttachmentState, DisplayIdentitySource,
    DisplayRouteState, DisplaySetupOptions, MediaSessionPhase, MediaSessionState, OperationStage,
    OperationState, PnpResolutionSource, RoutedDisplayMode, CAST_DISPLAY_PATH_PREFIX, MANAGER_PATH,
    MAX_MEDIA_ERROR_BYTES, OPERATION_PATH_PREFIX,
};
use thiserror::Error;
use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::Connection;
use zvariant::OwnedObjectPath;

use crate::caller::pin_bus_caller;
use crate::display::{CastDisplayId, DisplaySetupHandle, DisplaySetupSnapshot, DisplaySetupStage};
use crate::manager::{InventoryEvent, LifecycleEvent, ManagerHandle};

const TERMINAL_OPERATION_RETENTION: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ManagerInterface {
    manager: ManagerHandle,
}

impl ManagerInterface {
    pub fn new(manager: ManagerHandle) -> Self {
        Self { manager }
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.Manager")]
impl ManagerInterface {
    #[zbus(name = "GetVersion")]
    fn get_version(&self) -> ApiVersion {
        ApiVersion::CURRENT
    }

    #[zbus(name = "ListDevices")]
    async fn list_devices(&self) -> zbus::fdo::Result<DeviceSnapshot> {
        self.manager
            .list_devices()
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(name = "ListDisplays")]
    async fn list_displays(&self) -> zbus::fdo::Result<CastDisplaySnapshot> {
        let displays = self
            .manager
            .list_displays()
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let snapshot = CastDisplaySnapshot {
            displays: displays.iter().map(public_display).collect(),
        };
        snapshot
            .validate()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok(snapshot)
    }

    #[zbus(name = "AddDisplay")]
    async fn add_display(
        &self,
        device: DeviceSelection,
        options: DisplaySetupOptions,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        device
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("D-Bus caller has no sender".into()))?;
        let caller = pin_bus_caller(connection, sender)
            .await
            .map_err(|error| zbus::fdo::Error::AccessDenied(error.to_string()))?;
        let operation = self
            .manager
            .start_display_setup(device, None, caller, options.audio_enabled)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let path = operation_path(operation.display_id())?;
        let was_added = match object_server
            .at(
                path.clone(),
                OperationInterface::new(self.manager.clone(), operation.clone()),
            )
            .await
        {
            Ok(was_added) => was_added,
            Err(error) => {
                operation.cancel();
                let cleanup_manager = self.manager.clone();
                let cleanup_operation = operation.clone();
                tokio::spawn(async move {
                    retire_unpublished_operation(cleanup_manager, cleanup_operation).await;
                });
                return Err(zbus::fdo::Error::Failed(error.to_string()));
            }
        };
        if was_added {
            let signal_connection = connection.clone();
            let signal_path = path.clone();
            let signal_manager = self.manager.clone();
            tokio::spawn(async move {
                if let Err(error) = emit_operation_states(
                    &signal_connection,
                    signal_path,
                    signal_manager,
                    operation,
                )
                .await
                {
                    tracing::warn!(%error, "display-setup operation signal task stopped");
                }
            });
        }
        Ok(path)
    }

    #[zbus(name = "RemoveDisplay")]
    async fn remove_display(&self, display_id: String) -> zbus::fdo::Result<()> {
        let display_id = display_id
            .parse::<CastDisplayId>()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        self.manager
            .remove_display(display_id)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal, name = "DeviceAdded")]
    async fn device_added(
        emitter: &SignalEmitter<'_>,
        inventory_revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceChanged")]
    async fn device_changed(
        emitter: &SignalEmitter<'_>,
        inventory_revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceRemoved")]
    async fn device_removed(
        emitter: &SignalEmitter<'_>,
        inventory_revision: u64,
        backend_id: String,
        device_id: String,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayAdded")]
    async fn display_added(
        emitter: &SignalEmitter<'_>,
        display: CastDisplayInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayRemoved")]
    async fn display_removed(emitter: &SignalEmitter<'_>, display_id: String) -> zbus::Result<()>;
}

async fn retire_unpublished_operation(manager: ManagerHandle, operation: DisplaySetupHandle) {
    let mut status = operation.subscribe();
    while !status.borrow().stage.is_terminal() {
        if status.changed().await.is_err() {
            return;
        }
    }
    if let Err(error) = manager
        .forget_display_setup_operation(operation.display_id())
        .await
    {
        tracing::warn!(%error, "forget unpublished terminal setup operation");
    }
}

#[derive(Debug, Clone)]
struct OperationInterface {
    manager: ManagerHandle,
    operation: DisplaySetupHandle,
}

impl OperationInterface {
    fn new(manager: ManagerHandle, operation: DisplaySetupHandle) -> Self {
        Self { manager, operation }
    }
}

#[derive(Debug, Clone)]
struct CastDisplayInterface {
    manager: ManagerHandle,
    display_id: CastDisplayId,
    info: CastDisplayInfo,
    state: CastDisplayState,
}

impl CastDisplayInterface {
    fn new(
        manager: ManagerHandle,
        display_id: CastDisplayId,
        info: CastDisplayInfo,
        state: CastDisplayState,
    ) -> Self {
        Self {
            manager,
            display_id,
            info,
            state,
        }
    }
}

#[derive(Debug, Clone)]
struct MediaSessionInterface {
    state: MediaSessionState,
}

impl MediaSessionInterface {
    fn new(state: MediaSessionState) -> Self {
        Self { state }
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.CastDisplay")]
impl CastDisplayInterface {
    #[zbus(name = "GetInfo")]
    fn get_info(&self) -> CastDisplayInfo {
        self.info.clone()
    }

    #[zbus(name = "GetState")]
    fn get_state(&self) -> CastDisplayState {
        self.state.clone()
    }

    #[zbus(name = "Remove")]
    async fn remove(&self) -> zbus::fdo::Result<()> {
        self.manager
            .remove_display(self.display_id)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal, name = "Removed")]
    async fn removed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "StateChanged")]
    async fn state_changed(
        emitter: &SignalEmitter<'_>,
        state: CastDisplayState,
    ) -> zbus::Result<()>;
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.MediaSession")]
impl MediaSessionInterface {
    #[zbus(name = "GetState")]
    fn get_state(&self) -> MediaSessionState {
        self.state.clone()
    }

    #[zbus(signal, name = "StateChanged")]
    async fn state_changed(
        emitter: &SignalEmitter<'_>,
        state: MediaSessionState,
    ) -> zbus::Result<()>;
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.Operation")]
impl OperationInterface {
    #[zbus(name = "GetState")]
    fn get_state(&self) -> OperationState {
        public_operation_state(&self.operation.snapshot())
    }

    #[zbus(name = "Cancel")]
    async fn cancel(&self) -> zbus::fdo::Result<bool> {
        self.manager
            .cancel_display_setup(self.operation.display_id())
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal, name = "StateChanged")]
    async fn state_changed(emitter: &SignalEmitter<'_>, state: OperationState) -> zbus::Result<()>;
}

fn operation_path(display_id: CastDisplayId) -> zbus::fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(format!(
        "{OPERATION_PATH_PREFIX}/{}",
        display_id.object_segment()
    ))
    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

fn display_path(display_id: CastDisplayId) -> zbus::fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(format!(
        "{CAST_DISPLAY_PATH_PREFIX}/{}",
        display_id.object_segment()
    ))
    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

fn public_operation_state(snapshot: &DisplaySetupSnapshot) -> OperationState {
    OperationState {
        display_id: snapshot.display_id.to_string(),
        stage: match snapshot.stage {
            DisplaySetupStage::Validating => OperationStage::Validating,
            DisplaySetupStage::Authorizing => OperationStage::Authorizing,
            DisplaySetupStage::PreparingDevice => OperationStage::PreparingDevice,
            DisplaySetupStage::Attaching => OperationStage::Attaching,
            DisplaySetupStage::Added => OperationStage::Added,
            DisplaySetupStage::Cancelled => OperationStage::Cancelled,
            DisplaySetupStage::Failed => OperationStage::Failed,
        },
        error_code: snapshot.error_code,
        error: snapshot.error.clone().unwrap_or_default(),
    }
}

fn public_display(display: &crate::display::AddedCastDisplaySnapshot) -> CastDisplayInfo {
    let identity = &display.prepared.capabilities().display_identity;
    let numeric_identity = display.prepared.generated_edid().identity();
    CastDisplayInfo {
        display_id: display.display_id.to_string(),
        backend_id: display.device.backend_id.clone(),
        device_id: display.device.device_id.clone(),
        display_name: display.device.display_name.clone(),
        manufacturer_name: identity.manufacturer_name.clone().unwrap_or_default(),
        manufacturer_source: public_identity_source(identity.manufacturer_source),
        product_name: identity.product_name.clone().unwrap_or_default(),
        product_source: public_identity_source(identity.product_source),
        pnp_id: display.prepared.pnp_resolution().pnp_id.to_string(),
        pnp_resolution_source: match display.prepared.pnp_resolution().source {
            pronk_core::identity::PnpResolutionSource::AuthenticatedPnpId => {
                PnpResolutionSource::AuthenticatedPnpId
            }
            pronk_core::identity::PnpResolutionSource::ExactName => PnpResolutionSource::ExactName,
            pronk_core::identity::PnpResolutionSource::LegalSuffixName => {
                PnpResolutionSource::LegalSuffixName
            }
            pronk_core::identity::PnpResolutionSource::ReviewedAlias => {
                PnpResolutionSource::ReviewedAlias
            }
            pronk_core::identity::PnpResolutionSource::SynthesizerFallback => {
                PnpResolutionSource::SynthesizerFallback
            }
        },
        connector_id: display.output.connector_id,
        connector_name: display.output.connector_name.clone(),
        output_index: display.output.id.output_index,
        product_code: numeric_identity.product_code,
        serial: numeric_identity.serial,
        attachment_state: public_attachment_state(display.runtime.attachment),
    }
}

fn public_display_state(display: &crate::display::AddedCastDisplaySnapshot) -> CastDisplayState {
    CastDisplayState {
        revision: display.state_revision,
        device: display.device.clone(),
        attachment_state: public_attachment_state(display.runtime.attachment),
        route_state: match display.runtime.route {
            crate::display_state::RouteState::Disabled => DisplayRouteState::Disabled,
            crate::display_state::RouteState::Active(_) => DisplayRouteState::Active,
        },
        routed_mode: match display.runtime.route {
            crate::display_state::RouteState::Disabled => None,
            crate::display_state::RouteState::Active(route) => Some(RoutedDisplayMode {
                width: route.mode.width,
                height: route.mode.height,
                refresh_millihz: route.mode.refresh_millihz,
                flags: route.mode.flags,
            }),
        },
    }
}

fn public_media_session_state(
    display: &crate::display::AddedCastDisplaySnapshot,
) -> MediaSessionState {
    let phase = match display.runtime.media {
        crate::display_state::MediaState::Idle => MediaSessionPhase::Inactive,
        crate::display_state::MediaState::StartingCapture
        | crate::display_state::MediaState::StartingMedia => MediaSessionPhase::Starting,
        crate::display_state::MediaState::Running => MediaSessionPhase::Running,
        crate::display_state::MediaState::Suspended => MediaSessionPhase::Suspended,
        crate::display_state::MediaState::Reconfiguring
        | crate::display_state::MediaState::Reconnecting => MediaSessionPhase::Recovering,
        crate::display_state::MediaState::Stopping => MediaSessionPhase::Stopping,
        crate::display_state::MediaState::Failed => MediaSessionPhase::Failed,
    };
    let error = if phase == MediaSessionPhase::Failed {
        public_media_error(display.runtime.last_error.as_deref())
    } else {
        String::new()
    };
    let state = MediaSessionState {
        revision: display.state_revision,
        phase,
        media_generation: display.runtime.media_generation,
        audio_enabled: display.prepared.audio_enabled(),
        error,
    };
    debug_assert!(state.validate().is_ok());
    state
}

fn public_media_error(error: Option<&str>) -> String {
    const MISSING_DIAGNOSTIC: &str = "media session failed without diagnostic detail";

    let mut error = error
        .unwrap_or(MISSING_DIAGNOSTIC)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned();
    if error.is_empty() {
        return MISSING_DIAGNOSTIC.into();
    }
    if error.len() > MAX_MEDIA_ERROR_BYTES {
        let mut boundary = MAX_MEDIA_ERROR_BYTES;
        while !error.is_char_boundary(boundary) {
            boundary -= 1;
        }
        error.truncate(boundary);
    }
    error
}

fn same_media_observation(left: &MediaSessionState, right: &MediaSessionState) -> bool {
    left.phase == right.phase
        && left.media_generation == right.media_generation
        && left.audio_enabled == right.audio_enabled
        && left.error == right.error
}

fn public_attachment_state(state: crate::display_state::AttachmentState) -> DisplayAttachmentState {
    match state {
        crate::display_state::AttachmentState::Attached => DisplayAttachmentState::Attached,
        crate::display_state::AttachmentState::Detached => DisplayAttachmentState::Detached,
        crate::display_state::AttachmentState::Unknown => DisplayAttachmentState::Unknown,
    }
}

fn public_identity_source(source: pronk_backend_protocol::IdentitySource) -> DisplayIdentitySource {
    match source {
        pronk_backend_protocol::IdentitySource::Absent => DisplayIdentitySource::Absent,
        pronk_backend_protocol::IdentitySource::SetupEndpoint => {
            DisplayIdentitySource::SetupEndpoint
        }
        pronk_backend_protocol::IdentitySource::AuthenticatedDeviceInfo => {
            DisplayIdentitySource::AuthenticatedDeviceInfo
        }
        pronk_backend_protocol::IdentitySource::DiscoveryAdvertisement => {
            DisplayIdentitySource::DiscoveryAdvertisement
        }
    }
}

async fn emit_operation_states(
    connection: &Connection,
    path: OwnedObjectPath,
    manager: ManagerHandle,
    operation: DisplaySetupHandle,
) -> Result<(), OperationSignalError> {
    let emitter = SignalEmitter::new(connection, path.clone())
        .map_err(OperationSignalError::Emitter)?
        .into_owned();
    let mut status = operation.subscribe();
    while !status.borrow().stage.is_terminal() {
        status
            .changed()
            .await
            .map_err(|_| OperationSignalError::StatusClosed)?;
        let state = public_operation_state(&status.borrow());
        OperationInterface::state_changed(&emitter, state)
            .await
            .map_err(OperationSignalError::Emit)?;
    }
    tokio::time::sleep(TERMINAL_OPERATION_RETENTION).await;
    if manager
        .forget_display_setup_operation(operation.display_id())
        .await
        .map_err(|_| OperationSignalError::ManagerStopped)?
    {
        match connection
            .object_server()
            .remove::<OperationInterface, _>(path)
            .await
        {
            Ok(_) | Err(zbus::Error::InterfaceNotFound) => {}
            Err(error) => return Err(OperationSignalError::Remove(error)),
        }
    }
    Ok(())
}

pub async fn register_manager(
    connection: &Connection,
    manager: ManagerHandle,
) -> Result<(), zbus::Error> {
    connection
        .object_server()
        .at(MANAGER_PATH, ManagerInterface::new(manager))
        .await
        .map(|_| ())
}

pub async fn emit_inventory_events(
    connection: &Connection,
    mut events: mpsc::Receiver<InventoryEvent>,
) -> Result<(), InventorySignalError> {
    let emitter = SignalEmitter::new(connection, MANAGER_PATH)
        .map_err(InventorySignalError::Emitter)?
        .into_owned();
    while let Some(event) = events.recv().await {
        match event {
            InventoryEvent::DeviceAdded {
                inventory_revision,
                device,
            } => ManagerInterface::device_added(&emitter, inventory_revision, device)
                .await
                .map_err(InventorySignalError::Emit)?,
            InventoryEvent::DeviceChanged {
                inventory_revision,
                device,
            } => ManagerInterface::device_changed(&emitter, inventory_revision, device)
                .await
                .map_err(InventorySignalError::Emit)?,
            InventoryEvent::DeviceRemoved {
                inventory_revision,
                backend_id,
                device_id,
            } => ManagerInterface::device_removed(
                &emitter,
                inventory_revision,
                backend_id,
                device_id,
            )
            .await
            .map_err(InventorySignalError::Emit)?,
        }
    }
    Ok(())
}

pub async fn serve_lifecycle_events(
    connection: &Connection,
    manager: ManagerHandle,
    mut events: mpsc::UnboundedReceiver<LifecycleEvent>,
) -> Result<(), LifecycleSignalError> {
    let manager_emitter = SignalEmitter::new(connection, MANAGER_PATH)
        .map_err(LifecycleSignalError::Emitter)?
        .into_owned();
    while let Some(event) = events.recv().await {
        match event {
            LifecycleEvent::DisplayAdded(snapshot) => {
                let display_id = snapshot.display_id;
                let info = public_display(&snapshot);
                let state = public_display_state(&snapshot);
                let media_state = public_media_session_state(&snapshot);
                let path = display_path(display_id).map_err(LifecycleSignalError::Method)?;
                connection
                    .object_server()
                    .at(
                        path.clone(),
                        CastDisplayInterface::new(manager.clone(), display_id, info.clone(), state),
                    )
                    .await
                    .map_err(LifecycleSignalError::RegisterDisplay)?;
                if let Err(error) = connection
                    .object_server()
                    .at(path.clone(), MediaSessionInterface::new(media_state))
                    .await
                {
                    if let Err(rollback) = connection
                        .object_server()
                        .remove::<CastDisplayInterface, _>(path)
                        .await
                    {
                        tracing::warn!(%display_id, %rollback, "roll back partial public display registration");
                    }
                    return Err(LifecycleSignalError::RegisterMediaSession(error));
                }
                ManagerInterface::display_added(&manager_emitter, info)
                    .await
                    .map_err(LifecycleSignalError::Emit)?;
            }
            LifecycleEvent::DisplayStateChanged(snapshot) => {
                let display_id = snapshot.display_id;
                let info = public_display(&snapshot);
                let state = public_display_state(&snapshot);
                let media_state = public_media_session_state(&snapshot);
                let path = display_path(display_id).map_err(LifecycleSignalError::Method)?;
                let display_interface = connection
                    .object_server()
                    .interface::<_, CastDisplayInterface>(path.clone())
                    .await
                    .map_err(LifecycleSignalError::LookupDisplay)?;
                {
                    let mut current = display_interface.get_mut().await;
                    current.info = info;
                    current.state = state.clone();
                }
                let media_interface = connection
                    .object_server()
                    .interface::<_, MediaSessionInterface>(path)
                    .await
                    .map_err(LifecycleSignalError::LookupMediaSession)?;
                let media_changed = {
                    let mut current = media_interface.get_mut().await;
                    if same_media_observation(&current.state, &media_state) {
                        false
                    } else {
                        current.state = media_state.clone();
                        true
                    }
                };
                CastDisplayInterface::state_changed(display_interface.signal_emitter(), state)
                    .await
                    .map_err(LifecycleSignalError::Emit)?;
                if media_changed {
                    MediaSessionInterface::state_changed(
                        media_interface.signal_emitter(),
                        media_state,
                    )
                    .await
                    .map_err(LifecycleSignalError::Emit)?;
                }
            }
            LifecycleEvent::DisplayRemoved { display_id } => {
                let path = display_path(display_id).map_err(LifecycleSignalError::Method)?;
                if let Ok(emitter) = SignalEmitter::new(connection, path.clone()) {
                    if let Err(error) = CastDisplayInterface::removed(&emitter).await {
                        tracing::warn!(display_id = %display_id, %error, "emit cast-display removal");
                    }
                }
                remove_interface_if_present::<MediaSessionInterface>(connection, path.clone())
                    .await?;
                remove_interface_if_present::<CastDisplayInterface>(connection, path).await?;
                let operation_path =
                    operation_path(display_id).map_err(LifecycleSignalError::Method)?;
                remove_interface_if_present::<OperationInterface>(connection, operation_path)
                    .await?;
                ManagerInterface::display_removed(&manager_emitter, display_id.to_string())
                    .await
                    .map_err(LifecycleSignalError::Emit)?;
            }
        }
    }
    Ok(())
}

async fn remove_interface_if_present<I>(
    connection: &Connection,
    path: OwnedObjectPath,
) -> Result<(), LifecycleSignalError>
where
    I: zbus::object_server::Interface,
{
    match connection.object_server().remove::<I, _>(path).await {
        Ok(_) | Err(zbus::Error::InterfaceNotFound) => Ok(()),
        Err(error) => Err(LifecycleSignalError::Remove(error)),
    }
}

#[derive(Debug, Error)]
pub enum InventorySignalError {
    #[error("create manager signal emitter: {0}")]
    Emitter(zbus::Error),
    #[error("emit manager inventory signal: {0}")]
    Emit(zbus::Error),
}

#[derive(Debug, Error)]
enum OperationSignalError {
    #[error("create operation signal emitter: {0}")]
    Emitter(zbus::Error),
    #[error("display-setup status channel closed before a terminal state")]
    StatusClosed,
    #[error("emit operation state signal: {0}")]
    Emit(zbus::Error),
    #[error("manager stopped before terminal operation retirement")]
    ManagerStopped,
    #[error("remove retired operation object: {0}")]
    Remove(zbus::Error),
}

#[derive(Debug, Error)]
pub enum LifecycleSignalError {
    #[error("create lifecycle signal emitter: {0}")]
    Emitter(zbus::Error),
    #[error("construct lifecycle object path: {0}")]
    Method(zbus::fdo::Error),
    #[error("register cast-display object: {0}")]
    RegisterDisplay(zbus::Error),
    #[error("register media-session interface: {0}")]
    RegisterMediaSession(zbus::Error),
    #[error("look up cast-display object: {0}")]
    LookupDisplay(zbus::Error),
    #[error("look up media-session interface: {0}")]
    LookupMediaSession(zbus::Error),
    #[error("emit lifecycle signal: {0}")]
    Emit(zbus::Error),
    #[error("remove lifecycle object: {0}")]
    Remove(zbus::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::StreamExt;
    use pronk_backend_protocol::{
        AudioProfile, DeviceCapabilities, DisplayIdentity, DisplayMode, IdentitySource,
        VideoProfile, SESSION_FEATURE_AUDIO,
    };
    use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID};
    use pronk_core::output::{CastKmsOutput, CastKmsOutputId, OutputConnection};
    use pronk_dbus::{
        CastDisplay1Proxy, DeviceAvailability, Manager1Proxy, MediaSession1Proxy, MediaSessionPhase,
    };
    use tokio::net::UnixStream;
    use zbus::connection::{AuthMechanism, Builder};
    use zbus::Guid;

    use super::*;
    use crate::display_state::{
        ActiveRoute, AttachmentState, DisplayTopology, MediaState, RouteTarget, RoutedMode,
    };
    use crate::manager::ManagerActor;
    use crate::preparation::PreparedCastDevice;
    use crate::test_support::UnreachableGrantProvider;

    fn device() -> DeviceInfo {
        DeviceInfo {
            backend_id: "mock".into(),
            device_id: "living-room".into(),
            display_name: "Living Room TV".into(),
            availability: DeviceAvailability::Available,
            connection_generation: 1,
            discovery_generation: 2,
            device_revision: 1,
            metadata: Vec::new(),
        }
    }

    fn added_display_snapshot(
        display_id: CastDisplayId,
    ) -> crate::display::AddedCastDisplaySnapshot {
        added_display_snapshot_with_audio(display_id, false)
    }

    fn added_display_snapshot_with_audio(
        display_id: CastDisplayId,
        audio_enabled: bool,
    ) -> crate::display::AddedCastDisplaySnapshot {
        let device = device();
        let resolver =
            PnpIdResolver::from_database("GGL\tGoogle Inc.\n", &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .unwrap();
        let prepared = PreparedCastDevice::from_capabilities(
            device.clone(),
            DeviceCapabilities {
                preparation_generation: 1,
                display_identity: DisplayIdentity {
                    manufacturer_name: Some("Google".into()),
                    manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
                    product_name: Some("Mock Display".into()),
                    product_source: IdentitySource::SetupEndpoint,
                    pnp_id: None,
                },
                modes: vec![DisplayMode {
                    width: 640,
                    height: 480,
                    refresh_millihz: 60_000,
                    flags: 0,
                }],
                video_profiles: vec![VideoProfile {
                    profile_id: "h264-high".into(),
                    codec: "h264".into(),
                    max_width: 1920,
                    max_height: 1080,
                    max_refresh_millihz: 60_000,
                }],
                audio_profiles: if audio_enabled {
                    vec![AudioProfile {
                        profile_id: "opus-stereo".into(),
                        codec: "opus".into(),
                        max_channels: 2,
                        sample_rates: vec![48_000],
                    }]
                } else {
                    Vec::new()
                },
                features: if audio_enabled {
                    SESSION_FEATURE_AUDIO
                } else {
                    0
                },
            },
            &resolver,
            audio_enabled,
        )
        .unwrap();
        let state_revision = device.device_revision;
        crate::display::AddedCastDisplaySnapshot {
            display_id,
            state_revision,
            device,
            prepared,
            output: CastKmsOutput {
                id: CastKmsOutputId {
                    device_path: "/sys/devices/virtual/castkms".into(),
                    output_index: 0,
                },
                node_path: "/dev/dri/card42".into(),
                device_major: 226,
                device_minor: 42,
                connector_id: 77,
                connector_name: "Virtual-1".into(),
                connection: OutputConnection::Connected,
            },
            grant_id: 9,
            grant_state: crate::display_state::DisplayGrantState::Active,
            runtime: crate::display_state::DisplayRuntimeState::attached(state_revision),
        }
    }

    #[test]
    fn public_media_projection_coalesces_internal_phases() {
        let display_id = CastDisplayId::generate().unwrap();
        let mut snapshot = added_display_snapshot_with_audio(display_id, true);
        let cases = [
            (MediaState::Idle, MediaSessionPhase::Inactive, 0),
            (MediaState::StartingCapture, MediaSessionPhase::Starting, 1),
            (MediaState::StartingMedia, MediaSessionPhase::Starting, 1),
            (MediaState::Running, MediaSessionPhase::Running, 1),
            (MediaState::Suspended, MediaSessionPhase::Suspended, 1),
            (MediaState::Reconfiguring, MediaSessionPhase::Recovering, 1),
            (MediaState::Reconnecting, MediaSessionPhase::Recovering, 1),
            (MediaState::Stopping, MediaSessionPhase::Stopping, 1),
            (MediaState::Failed, MediaSessionPhase::Failed, 1),
        ];
        for (internal, public, generation) in cases {
            snapshot.runtime.media = internal;
            snapshot.runtime.media_generation = generation;
            snapshot.runtime.last_error =
                (internal == MediaState::Failed).then(|| "transport failed".into());
            snapshot.state_revision = snapshot.state_revision.saturating_add(1);
            let projected = public_media_session_state(&snapshot);
            projected.validate().unwrap();
            assert_eq!(projected.phase, public);
            assert_eq!(projected.media_generation, generation);
            assert!(projected.audio_enabled);
        }

        snapshot.runtime.media = MediaState::Failed;
        snapshot.runtime.media_generation = 0;
        snapshot.runtime.last_error = Some(format!(
            "\n{}é",
            "x".repeat(pronk_dbus::MAX_MEDIA_ERROR_BYTES)
        ));
        snapshot.state_revision = snapshot.state_revision.saturating_add(1);
        let bounded = public_media_session_state(&snapshot);
        bounded.validate().unwrap();
        assert_eq!(bounded.error.len(), pronk_dbus::MAX_MEDIA_ERROR_BYTES);
        assert!(!bounded.error.chars().any(char::is_control));

        snapshot.runtime.last_error = Some("\n\t".into());
        snapshot.state_revision = snapshot.state_revision.saturating_add(1);
        let missing = public_media_session_state(&snapshot);
        missing.validate().unwrap();
        assert_eq!(
            missing.error,
            "media session failed without diagnostic detail"
        );
    }

    #[tokio::test]
    async fn public_interface_lists_and_emits_revisioned_devices() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let actor = ManagerActor::spawn(Vec::new(), Arc::new(UnreachableGrantProvider)).unwrap();
        let server = Builder::unix_stream(server_stream)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at(MANAGER_PATH, ManagerInterface::new(actor.handle()))
            .unwrap();
        let client = Builder::unix_stream(client_stream)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (server_connection, client_connection) =
            tokio::try_join!(server.build(), client.build()).unwrap();
        let proxy = Manager1Proxy::new(&client_connection).await.unwrap();

        assert_eq!(proxy.get_version().await.unwrap(), ApiVersion::CURRENT);
        assert_eq!(
            proxy.list_devices().await.unwrap(),
            DeviceSnapshot {
                inventory_revision: 0,
                devices: Vec::new(),
            }
        );
        assert_eq!(
            proxy.list_displays().await.unwrap(),
            CastDisplaySnapshot {
                displays: Vec::new(),
            }
        );

        let mut added = proxy.receive_device_added().await.unwrap();
        let (event_tx, event_rx) = mpsc::channel(1);
        let signal_task =
            tokio::spawn(async move { emit_inventory_events(&server_connection, event_rx).await });
        event_tx
            .send(InventoryEvent::DeviceAdded {
                inventory_revision: 1,
                device: device(),
            })
            .await
            .unwrap();
        let signal = added.next().await.unwrap();
        let args = signal.args().unwrap();
        assert_eq!(*args.inventory_revision(), 1);
        assert_eq!(args.device(), &device());

        drop(event_tx);
        signal_task.await.unwrap().unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cast_display_object_returns_bounded_info_and_removes_idempotently() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let actor = ManagerActor::spawn(Vec::new(), Arc::new(UnreachableGrantProvider)).unwrap();
        let display_id = CastDisplayId::generate().unwrap();
        let path = display_path(display_id).unwrap();
        let info = CastDisplayInfo {
            display_id: display_id.to_string(),
            backend_id: "mock".into(),
            device_id: "living-room".into(),
            display_name: "Living Room TV".into(),
            manufacturer_name: "Google".into(),
            manufacturer_source: DisplayIdentitySource::AuthenticatedDeviceInfo,
            product_name: "Mock Display".into(),
            product_source: DisplayIdentitySource::SetupEndpoint,
            pnp_id: "GGL".into(),
            pnp_resolution_source: PnpResolutionSource::LegalSuffixName,
            connector_id: 77,
            connector_name: "Virtual-1".into(),
            output_index: 0,
            product_code: 42,
            serial: 99,
            attachment_state: DisplayAttachmentState::Attached,
        };
        info.validate().unwrap();
        let state = CastDisplayState {
            revision: 1,
            device: device(),
            attachment_state: DisplayAttachmentState::Attached,
            route_state: DisplayRouteState::Disabled,
            routed_mode: None,
        };
        state.validate().unwrap();
        let server = Builder::unix_stream(server_stream)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at(
                path.clone(),
                CastDisplayInterface::new(actor.handle(), display_id, info.clone(), state.clone()),
            )
            .unwrap();
        let client = Builder::unix_stream(client_stream)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (_server_connection, client_connection) =
            tokio::try_join!(server.build(), client.build()).unwrap();
        let proxy = CastDisplay1Proxy::builder(&client_connection)
            .path(path)
            .unwrap()
            .build()
            .await
            .unwrap();

        assert_eq!(proxy.get_info().await.unwrap(), info);
        assert_eq!(proxy.get_state().await.unwrap(), state);
        proxy.remove().await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_events_register_signal_and_remove_cast_display_objects() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let actor = ManagerActor::spawn(Vec::new(), Arc::new(UnreachableGrantProvider)).unwrap();
        let server = Builder::unix_stream(server_stream)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at(MANAGER_PATH, ManagerInterface::new(actor.handle()))
            .unwrap();
        let client = Builder::unix_stream(client_stream)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (server_connection, client_connection) =
            tokio::try_join!(server.build(), client.build()).unwrap();
        let manager_proxy = Manager1Proxy::new(&client_connection).await.unwrap();
        let mut added_signals = manager_proxy.receive_display_added().await.unwrap();
        let mut removed_signals = manager_proxy.receive_display_removed().await.unwrap();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let lifecycle_manager = actor.handle();
        let lifecycle_task = tokio::spawn(async move {
            serve_lifecycle_events(&server_connection, lifecycle_manager, event_rx).await
        });
        let display_id = CastDisplayId::generate().unwrap();
        let snapshot = added_display_snapshot(display_id);
        let expected = public_display(&snapshot);
        let expected_state = public_display_state(&snapshot);
        event_tx
            .send(LifecycleEvent::DisplayAdded(Box::new(snapshot)))
            .unwrap();

        let added = added_signals.next().await.unwrap();
        assert_eq!(added.args().unwrap().display(), &expected);
        let path = display_path(display_id).unwrap();
        let display_proxy = CastDisplay1Proxy::builder(&client_connection)
            .path(path.clone())
            .unwrap()
            .build()
            .await
            .unwrap();
        let media_proxy = MediaSession1Proxy::builder(&client_connection)
            .path(path.clone())
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut object_removed = display_proxy.receive_removed().await.unwrap();
        assert_eq!(display_proxy.get_info().await.unwrap(), expected);
        assert_eq!(display_proxy.get_state().await.unwrap(), expected_state);
        let initial_media = media_proxy.get_state().await.unwrap();
        initial_media.validate().unwrap();
        assert_eq!(initial_media.phase, MediaSessionPhase::Inactive);
        assert_eq!(initial_media.media_generation, 0);
        assert!(!initial_media.audio_enabled);

        let mut state_changes = display_proxy.receive_state_changed().await.unwrap();
        let mut media_changes = media_proxy.receive_state_changed().await.unwrap();
        let mut changed_snapshot = added_display_snapshot(display_id);
        changed_snapshot.device.display_name = "Living Room TV renamed".into();
        changed_snapshot.device.availability = DeviceAvailability::Unavailable;
        changed_snapshot.device.device_revision = 2;
        changed_snapshot.runtime.observe_topology(DisplayTopology {
            attachment: AttachmentState::Attached,
            route: Some(ActiveRoute {
                target: RouteTarget::new(std::num::NonZeroU32::new(19).unwrap()),
                mode: RoutedMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            }),
        });
        changed_snapshot.state_revision = changed_snapshot.runtime.revision;
        let changed_info = public_display(&changed_snapshot);
        let changed_state = public_display_state(&changed_snapshot);
        event_tx
            .send(LifecycleEvent::DisplayStateChanged(Box::new(
                changed_snapshot.clone(),
            )))
            .unwrap();
        let changed = state_changes.next().await.unwrap();
        assert_eq!(changed.args().unwrap().state(), &changed_state);
        assert_eq!(changed_state.route_state, DisplayRouteState::Active);
        assert_eq!(changed_state.routed_mode.unwrap().width, 1920);
        assert_eq!(display_proxy.get_state().await.unwrap(), changed_state);
        assert_eq!(display_proxy.get_info().await.unwrap(), changed_info);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), media_changes.next())
                .await
                .is_err()
        );
        assert_eq!(media_proxy.get_state().await.unwrap(), initial_media);

        changed_snapshot
            .runtime
            .observe_media(1, crate::display_state::MediaState::Running, None);
        changed_snapshot.state_revision = changed_snapshot.runtime.revision;
        let expected_running = public_media_session_state(&changed_snapshot);
        event_tx
            .send(LifecycleEvent::DisplayStateChanged(Box::new(
                changed_snapshot,
            )))
            .unwrap();
        let display_media_change = state_changes.next().await.unwrap();
        assert_eq!(
            display_media_change.args().unwrap().state().revision,
            expected_running.revision
        );
        let media_change = media_changes.next().await.unwrap();
        assert_eq!(media_change.args().unwrap().state(), &expected_running);
        assert_eq!(expected_running.phase, MediaSessionPhase::Running);
        assert_eq!(expected_running.media_generation, 1);
        assert_eq!(media_proxy.get_state().await.unwrap(), expected_running);

        event_tx
            .send(LifecycleEvent::DisplayRemoved { display_id })
            .unwrap();
        object_removed.next().await.unwrap();
        let removed = removed_signals.next().await.unwrap();
        assert_eq!(
            removed.args().unwrap().display_id(),
            &display_id.to_string()
        );
        assert!(display_proxy.get_info().await.is_err());
        assert!(media_proxy.get_state().await.is_err());

        drop(event_tx);
        lifecycle_task.await.unwrap().unwrap();
        actor.shutdown().await.unwrap();
    }
}
