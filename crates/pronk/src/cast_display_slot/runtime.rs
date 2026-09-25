//! Resource-owning cast-display event loop and terminal cleanup.

use pronk_dbus::DeviceAvailability;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

use super::{
    apply_kernel_event, current_media_failure, media_policy_input, publish, publish_media_failure,
    update_device, CastDisplaySlotEvent, DeviceSessionAction, DeviceSessionPolicyState,
    SlotCommand,
};
use crate::device_recovery::{DeviceSessionRecoveryActor, DeviceSessionRecoveryEvent};
use crate::device_session_port::DeviceSessionStopReason;
use crate::display::{
    AddedCastDisplayResources, AddedCastDisplaySnapshot, CastDisplayId, RemoveCastDisplayError,
};
use crate::display_state::{DisplayTopology, MediaState};
use crate::kernel_display_port::{KernelDisplayEvent, KernelDisplayPort};
use crate::manager::CastDisplaySlotLease;
use crate::media_policy::{DisplayMediaPolicyActor, MediaPolicyEvent};
use crate::media_session::{MediaSessionSnapshot, MediaStopReason};

pub(super) async fn run_slot(
    resources: AddedCastDisplayResources,
    commands: mpsc::Receiver<SlotCommand>,
    owner_drop_signal: oneshot::Receiver<()>,
    state: watch::Sender<AddedCastDisplaySnapshot>,
    events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
) {
    SlotRuntime::new(resources, state, events)
        .run(commands, owner_drop_signal)
        .await;
}

struct SlotRuntime {
    display_id: CastDisplayId,
    slot: CastDisplaySlotLease,
    kernel: Box<dyn KernelDisplayPort>,
    device_session: DeviceSessionPolicyState,
    recovery: DeviceSessionRecoveryActor,
    media_policy: DisplayMediaPolicyActor,
    media_state: watch::Receiver<MediaSessionSnapshot>,
    media_events_open: bool,
    state: watch::Sender<AddedCastDisplaySnapshot>,
    events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
}

enum SlotExit {
    Remove {
        reason: DeviceSessionStopReason,
        response: oneshot::Sender<Result<(), RemoveCastDisplayError>>,
    },
    Terminal(String),
    Shutdown,
}

impl SlotRuntime {
    fn new(
        resources: AddedCastDisplayResources,
        state: watch::Sender<AddedCastDisplaySnapshot>,
        events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
    ) -> Self {
        let AddedCastDisplayResources {
            display_id,
            prepared,
            slot,
            media_driver,
            recovery_factory,
            session_replacement,
            initial_session_generation,
            session_events,
            kernel,
            ..
        } = resources;
        let device_session = DeviceSessionPolicyState::new(
            prepared.device(),
            state.borrow().device.availability == DeviceAvailability::Available,
            initial_session_generation.get(),
        );
        let recovery = DeviceSessionRecoveryActor::spawn(
            recovery_factory,
            session_replacement,
            prepared,
            initial_session_generation,
            session_events,
        )
        .expect("cast-display slot task runs inside Tokio");
        let media_policy = DisplayMediaPolicyActor::spawn(
            media_driver,
            media_policy_input(&state.borrow(), &device_session),
        )
        .expect("cast-display slot task runs inside Tokio");
        let media_state = media_policy.subscribe();
        Self {
            display_id,
            slot,
            kernel,
            device_session,
            recovery,
            media_policy,
            media_state,
            media_events_open: true,
            state,
            events,
        }
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<SlotCommand>,
        mut owner_drop_signal: oneshot::Receiver<()>,
    ) {
        let exit = loop {
            tokio::select! {
                biased;
                _ = &mut owner_drop_signal => break SlotExit::Shutdown,
                command = commands.recv() => match command {
                    Some(SlotCommand::UpdateDevice { device, response }) => {
                        let recovery_action = self.device_session.observe_device(&device);
                        let changed = update_device(&self.state, device);
                        if changed {
                            publish(&self.state, &self.events);
                        }
                        if changed || recovery_action.is_some() {
                            self.media_policy.observe(media_policy_input(&self.state.borrow(), &self.device_session));
                        }
                        let _ = response.send(changed);
                        match recovery_action {
                            Some(DeviceSessionAction::Cancel) => self.recovery.handle().cancel_phase(),
                            Some(DeviceSessionAction::Recover(device)) => {
                                match self.recovery.handle().recover(device.clone()).await {
                                    Ok(request_generation) => {
                                        self.device_session.begin_request(request_generation, &device);
                                    }
                                    Err(error) => {
                                        let diagnostic = format!("start Device-session recovery: {error}");
                                        publish_media_failure(&self.state, &self.events, &diagnostic);
                                        break SlotExit::Terminal(diagnostic);
                                    }
                                }
                            }
                            None => {}
                        }
                    }
                    Some(SlotCommand::Remove { reason, response }) => {
                        break SlotExit::Remove { reason, response };
                    }
                    None => break SlotExit::Shutdown,
                },
                event = self.kernel.next_event() => match event {
                    Ok(event) => {
                        let revoked = event == KernelDisplayEvent::Revoked;
                        let media_failure = current_media_failure(
                            self.state.borrow().runtime.media_generation(),
                            &event,
                        );
                        apply_kernel_event(&self.state, &self.events, event);
                        self.media_policy.observe(media_policy_input(&self.state.borrow(), &self.device_session));
                        if let Some(error) = media_failure {
                            let _ = self.media_policy.report_failure(error).await;
                        }
                        if revoked {
                            break SlotExit::Terminal("CastKMS grant was revoked".into());
                        }
                    }
                    Err(error) => {
                        let diagnostic = error.to_string();
                        self.state.send_modify(|snapshot| {
                            snapshot.runtime.observe_topology(DisplayTopology::Unknown);
                            let media_generation = snapshot.runtime.media_generation();
                            snapshot.runtime.observe_media(
                                media_generation,
                                MediaState::Failed,
                                Some(diagnostic.clone()),
                            );
                            snapshot.state_revision = snapshot.runtime.revision();
                        });
                        publish(&self.state, &self.events);
                        break SlotExit::Terminal(diagnostic);
                    }
                },
                event = self.recovery.next_event() => match event {
                    Some(DeviceSessionRecoveryEvent::Ready {
                        request_generation,
                        device,
                        session_generation,
                        retired_session_cleanup_error,
                    }) => {
                        if self.device_session.complete_request(
                            request_generation,
                            &device,
                            session_generation.get(),
                            &self.state.borrow().device,
                        ) {
                            if let Some(error) = retired_session_cleanup_error {
                                warn!(%self.display_id, %error, "retired Device session did not acknowledge final cleanup");
                            }
                            self.media_policy.observe(media_policy_input(&self.state.borrow(), &self.device_session));
                        }
                    }
                    Some(DeviceSessionRecoveryEvent::Failed {
                        request_generation,
                        device,
                        error,
                    }) => {
                        if self.device_session.fail_request(request_generation, &device) {
                            let diagnostic = format!("Device-session recovery failed: {error}");
                            publish_media_failure(&self.state, &self.events, &diagnostic);
                            break SlotExit::Terminal(diagnostic);
                        }
                    }
                    Some(DeviceSessionRecoveryEvent::TransportFailed {
                        session_generation,
                        error,
                    }) => {
                        if self.device_session.transport_failed(session_generation.get()) {
                            let diagnostic = format!("Device session transport failed: {error}");
                            publish_media_failure(&self.state, &self.events, &diagnostic);
                            self.media_policy.observe(media_policy_input(&self.state.borrow(), &self.device_session));
                            let current = self.state.borrow().device.clone();
                            if current.availability == DeviceAvailability::Available {
                                match self.recovery.handle().recover(current.clone()).await {
                                    Ok(request_generation) => {
                                        self.device_session.begin_request(request_generation, &current);
                                    }
                                    Err(recovery_error) => {
                                        warn!(%self.display_id, %recovery_error, "could not start recovery after Device-session transport failure");
                                        let terminal = format!(
                                            "start recovery after Device-session transport failure: {recovery_error}"
                                        );
                                        let _ = self.media_policy.report_failure(diagnostic).await;
                                        break SlotExit::Terminal(terminal);
                                    }
                                }
                            }
                            let _ = self.media_policy.report_failure(diagnostic).await;
                        }
                    }
                    None => {
                        let diagnostic = "Device-session recovery coordinator stopped".to_string();
                        publish_media_failure(&self.state, &self.events, &diagnostic);
                        break SlotExit::Terminal(diagnostic);
                    }
                },
                result = self.media_state.changed(), if self.media_events_open => {
                    if result.is_err() {
                        self.media_events_open = false;
                        continue;
                    }
                    let media = self.media_state.borrow_and_update().clone();
                    let changed = self.state.send_if_modified(|snapshot| {
                        if !snapshot.runtime.observe_media(
                            media.media_generation(),
                            media.state(),
                            media.last_error().map(str::to_owned),
                        ) {
                            return false;
                        }
                        snapshot.state_revision = snapshot.runtime.revision();
                        true
                    });
                    if changed {
                        publish(&self.state, &self.events);
                    }
                },
                event = self.media_policy.next_event() => {
                    break SlotExit::Terminal(match event {
                        Some(MediaPolicyEvent::RecoveryExhausted { error }) => error,
                        None => "media recovery policy stopped unexpectedly".into(),
                    });
                },
            }
        };

        commands.close();
        self.finish(exit).await;
    }

    async fn finish(self, exit: SlotExit) {
        let (reason, removal, terminal_error) = match exit {
            SlotExit::Remove { reason, response } => (reason, Some(response), None),
            SlotExit::Terminal(error) => {
                (DeviceSessionStopReason::DisplayRemoved, None, Some(error))
            }
            SlotExit::Shutdown => (DeviceSessionStopReason::DaemonShutdown, None, None),
        };

        let media_reason = match reason {
            DeviceSessionStopReason::DisplayRemoved => MediaStopReason::DisplayRemoved,
            DeviceSessionStopReason::DaemonShutdown => MediaStopReason::BackendShutdown,
        };
        let recovery_error = self
            .recovery
            .shutdown()
            .await
            .err()
            .map(|error| error.to_string());
        let media_error = self
            .media_policy
            .shutdown(media_reason)
            .await
            .err()
            .map(|error| error.to_string());
        let detach_error = self
            .kernel
            .detach()
            .await
            .err()
            .map(|error| error.to_string());
        let result = match (&recovery_error, &media_error, &detach_error) {
            (None, None, None) => Ok(()),
            _ => Err(RemoveCastDisplayError {
                recovery: recovery_error,
                media: media_error,
                detach: detach_error,
            }),
        };
        let cleanup_error = result.as_ref().err().map(ToString::to_string);
        // Release the manager's output reservation before publishing terminal
        // cleanup. The manager may immediately allow this Device to be set up
        // again after it consumes the event.
        drop(self.slot);
        if let Some(response) = removal {
            if response.send(result).is_err() {
                warn!(%self.display_id, "cast-display cleanup completed without a waiter");
            }
        } else if let Some(error) = terminal_error {
            let _ = self.events.send(CastDisplaySlotEvent::TerminalFailure {
                display_id: self.display_id,
                error,
                cleanup_error,
            });
        } else if let Some(error) = cleanup_error {
            warn!(%self.display_id, %error, "cast-display owner shutdown did not clean up completely");
        }
    }
}
