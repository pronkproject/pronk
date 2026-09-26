use pronk_dbus::MANAGER_PATH;
use thiserror::Error;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::Connection;
use zvariant::OwnedObjectPath;

use super::interfaces::{
    display_path, operation_path, CastDisplayInterface, ManagerInterface, MediaSessionInterface,
    OperationInterface,
};
use super::public_state::{
    public_display, public_display_state, public_media_session_state, public_operation_state,
    same_media_observation,
};
use crate::display::DisplaySetupHandle;
use crate::manager::{InventoryEvent, LifecycleEvent, ManagerHandle};

const TERMINAL_OPERATION_RETENTION: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) async fn emit_operation_states(
    connection: &Connection,
    path: OwnedObjectPath,
    manager: ManagerHandle,
    operation: DisplaySetupHandle,
) -> Result<(), OperationSignalError> {
    let emitter = SignalEmitter::new(connection, path.clone())
        .map_err(OperationSignalError::Emitter)?
        .into_owned();
    let mut status = operation.subscribe();
    // The setup task can reach its terminal state after AddDisplay registers
    // the object but before this spawned notifier first polls the watch
    // channel.  The caller may already have read a non-terminal snapshot, so
    // publish that terminal snapshot once instead of leaving it waiting for a
    // transition it cannot observe.
    let mut publish_current = status.borrow().stage().is_terminal();
    loop {
        if publish_current {
            let state = public_operation_state(&status.borrow());
            OperationInterface::state_changed(&emitter, state)
                .await
                .map_err(OperationSignalError::Emit)?;
        }
        if status.borrow().stage().is_terminal() {
            break;
        }
        status
            .changed()
            .await
            .map_err(|_| OperationSignalError::StatusClosed)?;
        publish_current = true;
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
            LifecycleEvent::Added(snapshot) => {
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
            LifecycleEvent::StateChanged(snapshot) => {
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
            LifecycleEvent::Removed { display_id } => {
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
pub(super) enum OperationSignalError {
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
