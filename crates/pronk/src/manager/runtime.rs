use std::collections::BTreeMap;

use pronk_dbus::DeviceSelection;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use super::backend_worker::{shutdown_workers, BackendWorker, BackendWorkerMessage};
use super::display_lifecycle::{
    handle_removal_join, handle_setup_join, spawn_display_removal, start_managed_display_setup,
    ManagedDisplayPhase, ManagedDisplayRecord, RemovalCompletion, RemovalRequest, SetupCompletion,
    TrackedDisplayTasks,
};
use super::inventory::{configured_device_update, AggregateInventory, ApplySupervisorOutcome};
use super::{
    InventoryEvent, LifecycleEvent, ManagerCommand, ManagerEventSinks, ManagerHandle,
    ManagerShutdownReport, ManagerTaskError, ReservedCastDisplayCore, ReservedCastDisplaySlot,
    ResolveDeviceError, ResolvedDeviceSelection, SelectionBackend,
};
use crate::cast_display_slot::CastDisplaySlotEvent;
use crate::device_session_port::DeviceSessionStopReason;
use crate::display::CastDisplayId;
use crate::slot::{OutputReservationRelease, OutputSlotPool};

pub(super) struct ManagerTaskContext {
    pub(super) commands: mpsc::Receiver<ManagerCommand>,
    pub(super) shutdown: oneshot::Receiver<oneshot::Sender<ManagerShutdownReport>>,
    pub(super) events: ManagerEventSinks,
    pub(super) backend_events: mpsc::Receiver<BackendWorkerMessage>,
    pub(super) reservation_releases: mpsc::UnboundedSender<OutputReservationRelease>,
    pub(super) reservation_release_events: mpsc::UnboundedReceiver<OutputReservationRelease>,
    pub(super) slot_events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
    pub(super) slot_event_rx: mpsc::UnboundedReceiver<CastDisplaySlotEvent>,
    pub(super) manager: ManagerHandle,
    pub(super) workers: Vec<BackendWorker>,
}

#[derive(Default)]
struct ManagerRuntimeState {
    inventory: AggregateInventory,
    output_slots: OutputSlotPool,
    records: BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    setup_tasks: TrackedDisplayTasks<SetupCompletion>,
    removal_tasks: TrackedDisplayTasks<RemovalCompletion>,
}

enum CommandFlow {
    Continue,
    Stop,
}

impl ManagerRuntimeState {
    fn resolve_live_device(
        &self,
        selection: &DeviceSelection,
        workers: &[BackendWorker],
    ) -> Result<ResolvedDeviceSelection, ResolveDeviceError> {
        let device = self.inventory.resolve_device(selection)?;
        let backend = workers
            .iter()
            .find(|worker| worker.backend_id == device.backend_id)
            .map(|worker| worker.handle.clone())
            .ok_or_else(|| ResolveDeviceError::BackendUnavailable {
                backend_id: device.backend_id.clone(),
            })?;
        Ok(ResolvedDeviceSelection {
            device,
            backend: SelectionBackend::Live(backend),
        })
    }

    fn handle_setup_completion(
        &mut self,
        joined: (
            CastDisplayId,
            Result<SetupCompletion, tokio::task::JoinError>,
        ),
        slot_events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
    ) -> Option<LifecycleEvent> {
        handle_setup_join(
            joined,
            &mut self.records,
            &mut self.removal_tasks,
            &self.inventory,
            slot_events,
        )
    }

    fn handle_removal_completion(
        &mut self,
        joined: (
            CastDisplayId,
            Result<RemovalCompletion, tokio::task::JoinError>,
        ),
    ) -> Option<LifecycleEvent> {
        handle_removal_join(joined, &mut self.records)
    }

    fn handle_command(
        &mut self,
        command: Option<ManagerCommand>,
        manager: &ManagerHandle,
        workers: &[BackendWorker],
        reservation_releases: &mpsc::UnboundedSender<OutputReservationRelease>,
    ) -> CommandFlow {
        match command {
            Some(ManagerCommand::ListDevices(response)) => {
                let _ = response.send(self.inventory.snapshot());
            }
            Some(ManagerCommand::ListDisplays(response)) => {
                let snapshots = self
                    .records
                    .values()
                    .filter_map(|record| match &record.phase {
                        ManagedDisplayPhase::Active(display) => Some(display.snapshot()),
                        _ => None,
                    })
                    .collect();
                let _ = response.send(snapshots);
            }
            Some(ManagerCommand::GetDisplay {
                display_id,
                response,
            }) => {
                let snapshot =
                    self.records
                        .get(&display_id)
                        .and_then(|record| match &record.phase {
                            ManagedDisplayPhase::Active(display) => Some(display.snapshot()),
                            _ => None,
                        });
                let _ = response.send(snapshot);
            }
            Some(ManagerCommand::ResolveDevice {
                selection,
                response,
            }) => {
                let result = self.resolve_live_device(&selection, workers);
                let _ = response.send(result);
            }
            Some(ManagerCommand::ReserveDisplaySlot {
                selection,
                outputs,
                preferred_output,
                response,
            }) => {
                let result = (|| {
                    let resolved = self.resolve_live_device(&selection, workers)?;
                    let device = resolved.device.clone();
                    let reservation = self.output_slots.reserve_where(
                        &device,
                        &outputs,
                        preferred_output.as_ref(),
                        |output| manager.kernel_session_provider.may_acquire(output),
                    )?;
                    Ok(ReservedCastDisplaySlot {
                        selection: resolved,
                        core: ReservedCastDisplayCore {
                            device,
                            selection_token: selection,
                            reservation: Some(reservation),
                            releases: reservation_releases.clone(),
                            manager_commands: manager.commands.clone(),
                        },
                    })
                })();
                let _ = response.send(result);
            }
            Some(ManagerCommand::StartDisplaySetup {
                selection,
                preferred_output,
                caller,
                audio_enabled,
                response,
            }) => {
                let result = start_managed_display_setup(
                    manager,
                    selection,
                    preferred_output,
                    caller,
                    audio_enabled,
                    &mut self.records,
                    &mut self.setup_tasks,
                );
                let _ = response.send(result);
            }
            Some(ManagerCommand::GetDisplaySetupOperation {
                display_id,
                response,
            }) => {
                let handle = self
                    .records
                    .get(&display_id)
                    .map(|record| record.handle.clone());
                let _ = response.send(handle);
            }
            Some(ManagerCommand::CancelDisplaySetup {
                display_id,
                response,
            }) => {
                let cancelled = self.records.get(&display_id).is_some_and(|record| {
                    if record.handle.snapshot().stage().is_terminal() {
                        false
                    } else {
                        record.handle.cancel();
                        true
                    }
                });
                let _ = response.send(cancelled);
            }
            Some(ManagerCommand::ForgetDisplaySetupOperation {
                display_id,
                response,
            }) => {
                let forgettable = self.records.get(&display_id).is_some_and(|record| {
                    record.handle.snapshot().stage().is_terminal()
                        && matches!(record.phase, ManagedDisplayPhase::Terminal)
                });
                if forgettable {
                    self.records.remove(&display_id);
                }
                let _ = response.send(forgettable);
            }
            Some(ManagerCommand::RemoveDisplay {
                display_id,
                response,
            }) => {
                let removal = match self.records.get_mut(&display_id) {
                    Some(record) => record.request_removal(response),
                    None => RemovalRequest::Complete(response),
                };
                match removal {
                    RemovalRequest::Start(display) => {
                        spawn_display_removal(display_id, display, &mut self.removal_tasks);
                    }
                    RemovalRequest::Queued => {}
                    RemovalRequest::Complete(response) => {
                        // Remove is idempotent, including after successful cleanup.
                        let _ = response.send(Ok(()));
                    }
                }
            }
            None => return CommandFlow::Stop,
        }
        CommandFlow::Continue
    }

    async fn handle_backend_message(
        &mut self,
        message: Option<BackendWorkerMessage>,
        events: &ManagerEventSinks,
    ) -> Result<(), ManagerTaskError> {
        match message {
            Some(BackendWorkerMessage::Event { backend_id, event }) => {
                match self.inventory.apply_supervisor_event(&backend_id, &event) {
                    Ok(ApplySupervisorOutcome::Changed(changes)) => {
                        publish_inventory_changes(changes, &self.records, events).await?;
                    }
                    Ok(ApplySupervisorOutcome::IgnoredStale) => {
                        debug!(backend_id, ?event, "ignored stale backend event");
                    }
                    Err(error) => {
                        warn!(backend_id, %error, "rejected backend inventory event");
                    }
                }
            }
            Some(BackendWorkerMessage::Stopped { backend_id, error }) => {
                warn!(backend_id, error, "backend supervisor stopped unexpectedly");
                let changes = self.inventory.mark_backend_unavailable(&backend_id)?;
                publish_inventory_changes(changes, &self.records, events).await?;
            }
            None => {
                let changes = self.inventory.mark_all_unavailable()?;
                publish_inventory_changes(changes, &self.records, events).await?;
            }
        }
        Ok(())
    }

    async fn handle_slot_event(
        &mut self,
        event: Option<CastDisplaySlotEvent>,
    ) -> Option<LifecycleEvent> {
        match event {
            Some(CastDisplaySlotEvent::StateChanged(snapshot)) => {
                if self
                    .records
                    .get(&snapshot.display_id)
                    .is_some_and(|record| matches!(record.phase, ManagedDisplayPhase::Active(_)))
                {
                    return Some(LifecycleEvent::DisplayStateChanged(snapshot));
                }
            }
            Some(CastDisplaySlotEvent::TerminalFailure {
                display_id,
                error,
                cleanup_error,
            }) => {
                if let Some(record) = self.records.get_mut(&display_id) {
                    let display = record.retire_active()?;
                    if let Err(join_error) = display.join_after_terminal().await {
                        warn!(%display_id, %join_error, "could not reap terminal cast-display owner");
                    }
                    if let Some(cleanup_error) = cleanup_error {
                        warn!(%display_id, %error, %cleanup_error, "removing cast display after terminal failure left cleanup errors");
                    } else {
                        warn!(%display_id, %error, "removed cast display after terminal failure");
                    }
                    return Some(LifecycleEvent::DisplayRemoved { display_id });
                }
            }
            None => {}
        }
        None
    }

    async fn shutdown_displays(
        &mut self,
        slot_events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
    ) -> BTreeMap<String, String> {
        for record in self.records.values() {
            if !record.handle.snapshot().stage().is_terminal() {
                record.handle.cancel();
            }
        }
        while let Some(joined) = self.setup_tasks.join_next().await {
            self.handle_setup_completion(joined, slot_events);
        }
        while let Some(joined) = self.removal_tasks.join_next().await {
            self.handle_removal_completion(joined);
        }

        let mut errors = BTreeMap::new();
        let mut removals = JoinSet::new();
        for (display_id, record) in std::mem::take(&mut self.records) {
            if let ManagedDisplayPhase::Active(display) = record.phase {
                removals.spawn(async move {
                    (
                        display_id,
                        display
                            .remove(DeviceSessionStopReason::DaemonShutdown)
                            .await
                            .map_err(|error| error.to_string()),
                    )
                });
            }
        }
        while let Some(joined) = removals.join_next().await {
            match joined {
                Ok((display_id, Err(error))) => {
                    errors.insert(format!("display:{display_id}"), error);
                }
                Ok((_, Ok(()))) => {}
                Err(error) => {
                    errors.insert(
                        format!("display-cleanup-task:{}", error.id()),
                        error.to_string(),
                    );
                }
            }
        }
        errors
    }
}

pub(super) async fn run_manager(
    context: ManagerTaskContext,
) -> Result<ManagerShutdownReport, ManagerTaskError> {
    let ManagerTaskContext {
        mut commands,
        shutdown: mut owner_shutdown,
        events,
        backend_events,
        reservation_releases,
        reservation_release_events: mut reservation_release_rx,
        slot_events,
        mut slot_event_rx,
        manager,
        workers,
    } = context;
    let mut state = ManagerRuntimeState::default();
    let mut backend_events = Some(backend_events);

    let shutdown_outcome = loop {
        tokio::select! {
            response = &mut owner_shutdown => break Ok(response.ok()),
            // Public requests must not starve setup, removal, backend, or
            // display-slot progress. Tokio's default randomized branch order
            // gives every continuously-ready input a chance to run.
            Some(release) = reservation_release_rx.recv() => {
                if !state.output_slots.release(&release) {
                    debug!(?release, "ignored stale display-slot release");
                }
            },
            command = commands.recv() => {
                match state.handle_command(command, &manager, &workers, &reservation_releases) {
                    CommandFlow::Continue => {},
                    CommandFlow::Stop => break Ok(None),
                }
            },
            joined = state.setup_tasks.join_next(), if !state.setup_tasks.is_empty() => {
                if let Some(event) = state.handle_setup_completion(
                    joined.expect("nonempty setup JoinSet returned no task"), &slot_events
                ) {
                    let _ = events.lifecycle.send(event);
                }
            },
            joined = state.removal_tasks.join_next(), if !state.removal_tasks.is_empty() => {
                if let Some(event) = state.handle_removal_completion(
                    joined.expect("nonempty removal JoinSet returned no task")
                ) {
                    let _ = events.lifecycle.send(event);
                }
            },
            message = async {
                match backend_events.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let closed = message.is_none();
                let result = tokio::select! {
                    biased;
                    response = &mut owner_shutdown => break Ok(response.ok()),
                    result = state.handle_backend_message(message, &events) => result,
                };
                match result {
                    Ok(()) if closed => backend_events = None,
                    Ok(()) => {},
                    Err(error) => break Err(error),
                }
            },
            event = slot_event_rx.recv() => {
                let handled = tokio::select! {
                    biased;
                    response = &mut owner_shutdown => break Ok(response.ok()),
                    handled = state.handle_slot_event(event) => handled,
                };
                if let Some(event) = handled {
                    let _ = events.lifecycle.send(event);
                }
            },
        }
    };

    commands.close();
    let display_cleanup_errors = state.shutdown_displays(&slot_events).await;

    let mut report = shutdown_workers(workers).await;
    report.errors.extend(display_cleanup_errors);
    match shutdown_outcome {
        Ok(Some(response)) => {
            let _ = response.send(report.clone());
            Ok(report)
        }
        Ok(None) => Ok(report),
        Err(error) => Err(error),
    }
}

async fn publish_inventory_changes(
    changes: Vec<InventoryEvent>,
    records: &BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    events: &ManagerEventSinks,
) -> Result<(), ManagerTaskError> {
    for change in changes {
        events
            .inventory
            .send(change.clone())
            .await
            .map_err(|_| ManagerTaskError::EventConsumerStopped)?;
        refresh_configured_displays(records, &change).await;
    }
    Ok(())
}

async fn refresh_configured_displays(
    records: &BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    event: &InventoryEvent,
) {
    for display in records.values().filter_map(|record| match &record.phase {
        ManagedDisplayPhase::Active(display) => Some(display),
        _ => None,
    }) {
        let handle = display.handle();
        let current = handle.snapshot().device;
        let Some(device) = configured_device_update(&current, event) else {
            continue;
        };
        if let Err(error) = handle.update_device(device).await {
            warn!(display_id = %handle.display_id(), %error, "failed to refresh configured Device state");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use pronk_backend_host::{BackendSupervisorEvent, DeviceInventorySnapshot};
    use pronk_backend_protocol::{BackendInfo, DeviceAvailability, DeviceInfo};
    use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID};

    use super::*;
    use crate::display::MediaRuntime;
    use crate::manager::SystemOutputInventoryProvider;
    use crate::test_support::UnreachableKernelSessionProvider;

    #[tokio::test]
    async fn owner_shutdown_interrupts_a_blocked_inventory_publish() {
        let (commands, command_rx) = mpsc::channel(1);
        let (backend_events, backend_rx) = mpsc::channel(1);
        let (inventory_events, inventory_rx) = mpsc::channel(1);
        inventory_events
            .send(InventoryEvent::DeviceRemoved {
                inventory_revision: 1,
                backend_id: "mock".into(),
                device_id: "old".into(),
            })
            .await
            .unwrap();
        let (lifecycle_events, _lifecycle_rx) = mpsc::unbounded_channel();
        let (reservation_releases, reservation_release_rx) = mpsc::unbounded_channel();
        let (slot_events, slot_event_rx) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let manager = ManagerHandle {
            commands,
            output_provider: Arc::new(SystemOutputInventoryProvider),
            kernel_session_provider: Arc::new(UnreachableKernelSessionProvider),
            pnp_resolver: Arc::new(
                PnpIdResolver::from_database("SON\tSony\n", &[], DEFAULT_SYNTHESIZER_PNP_ID)
                    .unwrap(),
            ),
            media_runtime: MediaRuntime::for_user(0),
        };
        let task = tokio::spawn(run_manager(ManagerTaskContext {
            commands: command_rx,
            shutdown: shutdown_rx,
            events: ManagerEventSinks {
                inventory: inventory_events,
                lifecycle: lifecycle_events,
            },
            backend_events: backend_rx,
            reservation_releases,
            reservation_release_events: reservation_release_rx,
            slot_events,
            slot_event_rx,
            manager,
            workers: Vec::new(),
        }));
        backend_events
            .send(BackendWorkerMessage::Event {
                backend_id: "mock".into(),
                event: BackendSupervisorEvent::Connected {
                    connection_generation: 1,
                    negotiated_minor: 0,
                    info: BackendInfo::new("mock", "Mock", "test", "mock", "development"),
                    inventory: DeviceInventorySnapshot {
                        discovery_generation: 1,
                        revision: 1,
                        devices: vec![DeviceInfo {
                            backend_id: "mock".into(),
                            device_id: "new".into(),
                            display_name: "New".into(),
                            availability: DeviceAvailability::Available,
                            metadata: Vec::new(),
                        }],
                    },
                },
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend_events.capacity() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let (response, wait) = oneshot::channel();
        shutdown.send(response).unwrap();
        let report = tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap()
            .unwrap();
        assert!(report.errors.is_empty());
        assert!(task.await.unwrap().is_ok());
        drop(inventory_rx);
    }
}
