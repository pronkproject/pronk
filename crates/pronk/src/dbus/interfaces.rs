use pronk_dbus::{
    ApiVersion, CastDisplayInfo, CastDisplaySnapshot, CastDisplayState, DeviceInfo,
    DeviceSelection, DeviceSnapshot, DisplaySetupOptions, MediaSessionState, OperationState,
    CAST_DISPLAY_PATH_PREFIX, OPERATION_PATH_PREFIX,
};
use zbus::message::Header;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::Connection;
use zvariant::OwnedObjectPath;

use super::public_state::{public_display, public_operation_state};
use super::signals::emit_operation_states;
use crate::caller::pin_bus_caller;
use crate::display::{CastDisplayId, DisplaySetupHandle};
use crate::manager::ManagerHandle;

#[derive(Debug, Clone)]
pub(super) struct ManagerInterface {
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
    async fn get_version(&self) -> zbus::fdo::Result<ApiVersion> {
        Ok(ApiVersion::CURRENT)
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
        let caller = pin_caller(&header, connection).await?;
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
    pub(super) async fn device_added(
        emitter: &SignalEmitter<'_>,
        inventory_revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceChanged")]
    pub(super) async fn device_changed(
        emitter: &SignalEmitter<'_>,
        inventory_revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceRemoved")]
    pub(super) async fn device_removed(
        emitter: &SignalEmitter<'_>,
        inventory_revision: u64,
        backend_id: String,
        device_id: String,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayAdded")]
    pub(super) async fn display_added(
        emitter: &SignalEmitter<'_>,
        display: CastDisplayInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayRemoved")]
    pub(super) async fn display_removed(
        emitter: &SignalEmitter<'_>,
        display_id: String,
    ) -> zbus::Result<()>;
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
pub(super) struct OperationInterface {
    manager: ManagerHandle,
    operation: DisplaySetupHandle,
}

impl OperationInterface {
    pub(super) fn new(manager: ManagerHandle, operation: DisplaySetupHandle) -> Self {
        Self { manager, operation }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CastDisplayInterface {
    manager: ManagerHandle,
    display_id: CastDisplayId,
    pub(super) info: CastDisplayInfo,
    pub(super) state: CastDisplayState,
}

impl CastDisplayInterface {
    pub(super) fn new(
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
pub(super) struct MediaSessionInterface {
    pub(super) state: MediaSessionState,
}

impl MediaSessionInterface {
    pub(super) fn new(state: MediaSessionState) -> Self {
        Self { state }
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.CastDisplay")]
impl CastDisplayInterface {
    #[zbus(name = "GetInfo")]
    async fn get_info(&self) -> zbus::fdo::Result<CastDisplayInfo> {
        Ok(self.info.clone())
    }

    #[zbus(name = "GetState")]
    async fn get_state(&self) -> zbus::fdo::Result<CastDisplayState> {
        Ok(self.state.clone())
    }

    #[zbus(name = "Remove")]
    async fn remove(&self) -> zbus::fdo::Result<()> {
        self.manager
            .remove_display(self.display_id)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal, name = "Removed")]
    pub(super) async fn removed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "StateChanged")]
    pub(super) async fn state_changed(
        emitter: &SignalEmitter<'_>,
        state: CastDisplayState,
    ) -> zbus::Result<()>;
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.MediaSession")]
impl MediaSessionInterface {
    #[zbus(name = "GetState")]
    async fn get_state(&self) -> zbus::fdo::Result<MediaSessionState> {
        Ok(self.state.clone())
    }

    #[zbus(signal, name = "StateChanged")]
    pub(super) async fn state_changed(
        emitter: &SignalEmitter<'_>,
        state: MediaSessionState,
    ) -> zbus::Result<()>;
}

#[zbus::interface(name = "io.github.pronkproject.Pronk1.Operation")]
impl OperationInterface {
    #[zbus(name = "GetState")]
    async fn get_state(&self) -> zbus::fdo::Result<OperationState> {
        Ok(public_operation_state(&self.operation.snapshot()))
    }

    #[zbus(name = "Cancel")]
    async fn cancel(&self) -> zbus::fdo::Result<bool> {
        self.manager
            .cancel_display_setup(self.operation.display_id())
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal, name = "StateChanged")]
    pub(super) async fn state_changed(
        emitter: &SignalEmitter<'_>,
        state: OperationState,
    ) -> zbus::Result<()>;
}

fn method_sender<'a>(header: &'a Header<'a>) -> zbus::fdo::Result<&'a zbus::names::UniqueName<'a>> {
    header
        .sender()
        .ok_or_else(|| zbus::fdo::Error::AccessDenied("D-Bus caller has no sender".into()))
}

async fn pin_caller(
    header: &Header<'_>,
    connection: &Connection,
) -> zbus::fdo::Result<pronk_core::session::PinnedCallerProcess> {
    let sender = method_sender(header)?;
    pin_bus_caller(connection, sender)
        .await
        .map(pronk_core::session::PinnedCallerSession::into_process)
        .map_err(|error| zbus::fdo::Error::AccessDenied(error.to_string()))
}

pub(super) fn operation_path(display_id: CastDisplayId) -> zbus::fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(format!(
        "{OPERATION_PATH_PREFIX}/{}",
        display_id.object_segment()
    ))
    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

pub(super) fn display_path(display_id: CastDisplayId) -> zbus::fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(format!(
        "{CAST_DISPLAY_PATH_PREFIX}/{}",
        display_id.object_segment()
    ))
    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}
