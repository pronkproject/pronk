use std::collections::{BTreeMap, HashMap};
use std::future::Future;

use pronk_core::output::CastKmsOutputId;
use pronk_core::session::PinnedCallerProcess;
use pronk_dbus::DeviceSelection;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::cast_display_slot::{CastDisplaySlotActor, CastDisplaySlotEvent};
use crate::device_session_port::DeviceSessionStopReason;
use crate::display::{
    AddedCastDisplay, CastDisplayId, DisplaySetupHandle, DisplaySetupOperationError,
};

use super::inventory::AggregateInventory;
use super::{LifecycleEvent, ManagerHandle, RemoveManagedDisplayError, StartDisplaySetupError};

const MAX_RETAINED_SETUP_OPERATIONS: usize = 128;

/// Keeps each task's display identity beside the task that owns it.
pub(super) struct TrackedDisplayTasks<T> {
    tasks: JoinSet<T>,
    display_ids: HashMap<tokio::task::Id, CastDisplayId>,
}

impl<T> Default for TrackedDisplayTasks<T> {
    fn default() -> Self {
        Self {
            tasks: JoinSet::new(),
            display_ids: HashMap::new(),
        }
    }
}

impl<T: Send + 'static> TrackedDisplayTasks<T> {
    pub(super) fn spawn(
        &mut self,
        display_id: CastDisplayId,
        task: impl Future<Output = T> + Send + 'static,
    ) {
        let abort = self.tasks.spawn(task);
        self.display_ids.insert(abort.id(), display_id);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(super) async fn join_next(
        &mut self,
    ) -> Option<(CastDisplayId, Result<T, tokio::task::JoinError>)> {
        let joined = self.tasks.join_next_with_id().await?;
        let task_id = match &joined {
            Ok((task_id, _)) => *task_id,
            Err(error) => error.id(),
        };
        let display_id = self
            .display_ids
            .remove(&task_id)
            .expect("display task identity must be registered before completion");
        Some((display_id, joined.map(|(_, completion)| completion)))
    }
}

#[derive(Debug)]
pub(super) struct ManagedDisplayRecord {
    // This target is claimed through setup, cancellation, and active removal.
    target: DeviceTarget,
    pub(super) handle: DisplaySetupHandle,
    pub(super) phase: ManagedDisplayPhase,
}

#[derive(Debug)]
pub(super) enum ManagedDisplayPhase {
    SettingUp,
    CancellingSetup {
        waiters: Vec<oneshot::Sender<Result<(), RemoveManagedDisplayError>>>,
    },
    Active(CastDisplaySlotActor),
    Removing {
        waiters: Vec<oneshot::Sender<Result<(), RemoveManagedDisplayError>>>,
    },
    // Keep the finished setup result visible without retaining target ownership.
    Terminal,
}

pub(super) enum RemovalRequest {
    Start(CastDisplaySlotActor),
    Queued,
    Complete(oneshot::Sender<Result<(), RemoveManagedDisplayError>>),
}

impl ManagedDisplayRecord {
    pub(super) fn request_removal(
        &mut self,
        response: oneshot::Sender<Result<(), RemoveManagedDisplayError>>,
    ) -> RemovalRequest {
        match std::mem::replace(&mut self.phase, ManagedDisplayPhase::Terminal) {
            ManagedDisplayPhase::SettingUp => {
                self.handle.cancel();
                self.phase = ManagedDisplayPhase::CancellingSetup {
                    waiters: vec![response],
                };
                RemovalRequest::Queued
            }
            ManagedDisplayPhase::CancellingSetup { mut waiters } => {
                waiters.push(response);
                self.phase = ManagedDisplayPhase::CancellingSetup { waiters };
                RemovalRequest::Queued
            }
            ManagedDisplayPhase::Active(display) => {
                self.phase = ManagedDisplayPhase::Removing {
                    waiters: vec![response],
                };
                RemovalRequest::Start(display)
            }
            ManagedDisplayPhase::Removing { mut waiters } => {
                waiters.push(response);
                self.phase = ManagedDisplayPhase::Removing { waiters };
                RemovalRequest::Queued
            }
            other => {
                self.phase = other;
                RemovalRequest::Complete(response)
            }
        }
    }

    pub(super) fn retire_active(&mut self) -> Option<CastDisplaySlotActor> {
        match std::mem::replace(&mut self.phase, ManagedDisplayPhase::Terminal) {
            ManagedDisplayPhase::Active(display) => Some(display),
            other => {
                self.phase = other;
                None
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SetupCompletion {
    result: Result<AddedCastDisplay, DisplaySetupOperationError>,
}

#[derive(Debug)]
pub(super) struct RemovalCompletion {
    pub(super) result: Result<(), String>,
}

pub(super) fn spawn_display_removal(
    display_id: CastDisplayId,
    display: CastDisplaySlotActor,
    removal_tasks: &mut TrackedDisplayTasks<RemovalCompletion>,
) {
    removal_tasks.spawn(display_id, async move {
        RemovalCompletion {
            result: display
                .remove(DeviceSessionStopReason::DisplayRemoved)
                .await
                .map_err(|error| error.to_string()),
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DeviceTarget {
    backend_id: String,
    device_id: String,
}

impl From<&DeviceSelection> for DeviceTarget {
    fn from(selection: &DeviceSelection) -> Self {
        Self {
            backend_id: selection.backend_id.clone(),
            device_id: selection.device_id.clone(),
        }
    }
}

pub(super) fn start_managed_display_setup(
    manager: &ManagerHandle,
    selection: DeviceSelection,
    preferred_output: Option<CastKmsOutputId>,
    caller: PinnedCallerProcess,
    audio_enabled: bool,
    records: &mut BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    setup_tasks: &mut TrackedDisplayTasks<SetupCompletion>,
) -> Result<DisplaySetupHandle, StartDisplaySetupError> {
    selection
        .validate()
        .map_err(|error| StartDisplaySetupError::InvalidSelection(error.to_string()))?;
    let target = DeviceTarget::from(&selection);
    if let Some(record) = records.values().find(|record| {
        record.target == target && !matches!(record.phase, ManagedDisplayPhase::Terminal)
    }) {
        return Ok(record.handle.clone());
    }

    if records.len() >= MAX_RETAINED_SETUP_OPERATIONS {
        return Err(StartDisplaySetupError::TooManyOperations);
    }

    let operation = manager
        .spawn_display_setup_operation(selection, preferred_output, caller, audio_enabled)
        .map_err(StartDisplaySetupError::Start)?;
    let display_id = operation.display_id();
    let handle = operation.handle();
    if records.contains_key(&display_id) {
        return Err(StartDisplaySetupError::IdentityCollision);
    }
    records.insert(
        display_id,
        ManagedDisplayRecord {
            target: target.clone(),
            handle: handle.clone(),
            phase: ManagedDisplayPhase::SettingUp,
        },
    );
    setup_tasks.spawn(display_id, async move {
        SetupCompletion {
            result: operation.finish().await,
        }
    });
    Ok(handle)
}

pub(super) fn handle_setup_join(
    (display_id, joined): (
        CastDisplayId,
        Result<SetupCompletion, tokio::task::JoinError>,
    ),
    records: &mut BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    removal_tasks: &mut TrackedDisplayTasks<RemovalCompletion>,
    inventory: &AggregateInventory,
    slot_events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
) -> Option<LifecycleEvent> {
    let completion = match joined {
        Ok(completion) => completion,
        Err(error) => {
            if let Some(record) = records.remove(&display_id) {
                if let ManagedDisplayPhase::CancellingSetup { waiters } = record.phase {
                    for waiter in waiters {
                        let _ =
                            waiter.send(Err(RemoveManagedDisplayError::Cleanup(error.to_string())));
                    }
                }
            }
            warn!(%error, %display_id, "manager-owned display setup task failed");
            return None;
        }
    };
    let Some(record) = records.get_mut(&display_id) else {
        warn!(%display_id, "completed display setup has no manager record");
        return None;
    };
    let cancellation_waiters =
        match std::mem::replace(&mut record.phase, ManagedDisplayPhase::Terminal) {
            ManagedDisplayPhase::CancellingSetup { waiters } => Some(waiters),
            phase => {
                record.phase = phase;
                None
            }
        };
    match completion.result {
        Ok(mut display) => {
            debug_assert_eq!(display.display_id(), display_id);
            display.update_device(inventory.configured_device(display.device()));
            let actor = match CastDisplaySlotActor::spawn(display, slot_events.clone()) {
                Ok(actor) => actor,
                Err(error) => {
                    record.phase = ManagedDisplayPhase::Terminal;
                    if let Some(waiters) = cancellation_waiters {
                        for waiter in waiters {
                            let _ = waiter
                                .send(Err(RemoveManagedDisplayError::Cleanup(error.to_string())));
                        }
                    }
                    warn!(%display_id, %error, "failed to start cast-display slot actor");
                    return None;
                }
            };
            let snapshot = actor.snapshot();
            if let Some(waiters) = cancellation_waiters {
                record.phase = ManagedDisplayPhase::Removing { waiters };
                spawn_display_removal(display_id, actor, removal_tasks);
            } else {
                record.phase = ManagedDisplayPhase::Active(actor);
            }
            Some(LifecycleEvent::DisplayAdded(Box::new(snapshot)))
        }
        Err(error) => {
            record.phase = ManagedDisplayPhase::Terminal;
            if let Some(waiters) = cancellation_waiters {
                let result = if matches!(error, DisplaySetupOperationError::Setup(_)) {
                    Ok(())
                } else {
                    Err(RemoveManagedDisplayError::Cleanup(error.to_string()))
                };
                for waiter in waiters {
                    let _ = waiter.send(result.clone());
                }
            }
            debug!(%display_id, %error, "display setup reached a terminal non-added state");
            None
        }
    }
}

pub(super) fn handle_removal_join(
    (display_id, joined): (
        CastDisplayId,
        Result<RemovalCompletion, tokio::task::JoinError>,
    ),
    records: &mut BTreeMap<CastDisplayId, ManagedDisplayRecord>,
) -> Option<LifecycleEvent> {
    let result = match joined {
        Ok(completion) => completion.result,
        Err(error) => Err(error.to_string()),
    };
    let Some(record) = records.remove(&display_id) else {
        warn!(%display_id, "completed display removal has no manager record");
        return None;
    };
    let response = result.map_err(RemoveManagedDisplayError::Cleanup);
    if let ManagedDisplayPhase::Removing { waiters } = record.phase {
        for waiter in waiters {
            let _ = waiter.send(response.clone());
        }
    }
    Some(LifecycleEvent::DisplayRemoved { display_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::DisplaySetupError;

    #[tokio::test]
    async fn failed_task_keeps_its_display_identity() {
        let display_id = CastDisplayId::generate().unwrap();
        let mut tasks = TrackedDisplayTasks::<()>::default();
        tasks.spawn(display_id, std::future::pending());
        tasks.tasks.abort_all();

        let (failed_display, result) = tasks.join_next().await.unwrap();
        assert_eq!(failed_display, display_id);
        assert!(result.unwrap_err().is_cancelled());
        assert!(tasks.is_empty());
        assert!(tasks.display_ids.is_empty());
    }

    #[tokio::test]
    async fn removing_during_setup_waits_for_setup_cleanup() {
        let display_id = CastDisplayId::generate().unwrap();
        let (handle, cancellation) = DisplaySetupHandle::test_pending(display_id);
        let mut record = ManagedDisplayRecord {
            target: DeviceTarget {
                backend_id: "backend".into(),
                device_id: "device".into(),
            },
            handle,
            phase: ManagedDisplayPhase::SettingUp,
        };
        let (first, mut first_reply) = oneshot::channel();
        let (second, second_reply) = oneshot::channel();
        assert!(matches!(
            record.request_removal(first),
            RemovalRequest::Queued
        ));
        assert!(matches!(
            record.request_removal(second),
            RemovalRequest::Queued
        ));
        assert!(cancellation.is_cancelled());
        assert!(!matches!(record.phase, ManagedDisplayPhase::Terminal));
        assert!(matches!(
            first_reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let mut records = BTreeMap::from([(display_id, record)]);
        let mut removal_tasks = TrackedDisplayTasks::default();
        let (slot_events, _slot_event_rx) = mpsc::unbounded_channel();
        let event = handle_setup_join(
            (
                display_id,
                Ok(SetupCompletion {
                    result: Err(DisplaySetupOperationError::Setup(
                        DisplaySetupError::Cancelled,
                    )),
                }),
            ),
            &mut records,
            &mut removal_tasks,
            &AggregateInventory::default(),
            &slot_events,
        );
        assert!(event.is_none());
        assert!(matches!(
            records[&display_id].phase,
            ManagedDisplayPhase::Terminal
        ));
        assert!(first_reply.await.unwrap().is_ok());
        assert!(second_reply.await.unwrap().is_ok());
    }
}
