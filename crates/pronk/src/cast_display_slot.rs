//! Per-display aggregate actor.
//!
//! The manager owns this aggregate but interacts through its cheap handle. The
//! actor alone mutates the configured Device projection and kernel-derived
//! attachment/route state, and it owns ordered Device-session/kernel teardown.

use pronk_dbus::{DeviceAvailability, DeviceInfo};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::device_recovery::{DeviceSessionRecoveryActor, DeviceSessionRecoveryEvent};
use crate::device_session_port::DeviceSessionStopReason;
use crate::display::{
    AddedCastDisplay, AddedCastDisplayResources, AddedCastDisplaySnapshot, CastDisplayId,
    RemoveCastDisplayError,
};
use crate::display_state::{
    AttachmentState, DisplayGrantState, DisplayRuntimeState, DisplayTopology, MediaState,
};
use crate::kernel_display_port::KernelDisplayEvent;
use crate::media_policy::{DisplayMediaPolicyActor, MediaPolicyEvent, MediaPolicyInput};
use crate::media_session::{MediaRoute, MediaStopReason};

const SLOT_COMMAND_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastDisplaySlotEvent {
    StateChanged(Box<AddedCastDisplaySnapshot>),
    TerminalFailure {
        display_id: CastDisplayId,
        error: String,
        cleanup_error: Option<String>,
    },
}

pub struct CastDisplaySlotActor {
    handle: CastDisplaySlotHandle,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for CastDisplaySlotActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CastDisplaySlotActor")
            .field("display_id", &self.handle.display_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct CastDisplaySlotHandle {
    display_id: CastDisplayId,
    commands: mpsc::Sender<SlotCommand>,
    state: watch::Receiver<AddedCastDisplaySnapshot>,
}

impl CastDisplaySlotHandle {
    pub fn display_id(&self) -> CastDisplayId {
        self.display_id
    }

    pub fn snapshot(&self) -> AddedCastDisplaySnapshot {
        self.state.borrow().clone()
    }

    pub async fn update_device(&self, device: DeviceInfo) -> Result<bool, CastDisplaySlotError> {
        let (response, reply) = oneshot::channel();
        self.commands
            .send(SlotCommand::UpdateDevice { device, response })
            .await
            .map_err(|_| CastDisplaySlotError::Stopped)?;
        reply.await.map_err(|_| CastDisplaySlotError::Stopped)
    }
}

impl CastDisplaySlotActor {
    pub fn spawn(
        display: AddedCastDisplay,
        events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
    ) -> Result<Self, CastDisplaySlotError> {
        tokio::runtime::Handle::try_current().map_err(|_| CastDisplaySlotError::NoRuntime)?;
        let resources = display.into_resources();
        let initial = initial_snapshot(&resources);
        let display_id = resources.display_id;
        let (commands, command_rx) = mpsc::channel(SLOT_COMMAND_CAPACITY);
        let (state_tx, state) = watch::channel(initial);
        let task = tokio::spawn(run_slot(resources, command_rx, state_tx, events));
        Ok(Self {
            handle: CastDisplaySlotHandle {
                display_id,
                commands,
                state,
            },
            task: Some(task),
        })
    }

    pub fn handle(&self) -> CastDisplaySlotHandle {
        self.handle.clone()
    }

    pub fn snapshot(&self) -> AddedCastDisplaySnapshot {
        self.handle.snapshot()
    }

    pub async fn remove(
        mut self,
        reason: DeviceSessionStopReason,
    ) -> Result<(), CastDisplaySlotActorError> {
        let (response, reply) = oneshot::channel();
        self.handle
            .commands
            .send(SlotCommand::Remove { reason, response })
            .await
            .map_err(|_| CastDisplaySlotActorError::Stopped)?;
        let cleanup = reply
            .await
            .map_err(|_| CastDisplaySlotActorError::Stopped)?;
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| CastDisplaySlotActorError::Join(error.to_string()))?;
        }
        cleanup.map_err(CastDisplaySlotActorError::Cleanup)
    }

    /// Join a slot task after it has published a terminal event.
    ///
    /// Terminal events are emitted only after the task has released every
    /// owned media, Device-session, kernel, and output-reservation resource.
    /// Joining here lets the manager reap that completed owner instead of
    /// relying on the actor's abort-on-drop safety net.
    pub(crate) async fn join_after_terminal(mut self) -> Result<(), CastDisplaySlotActorError> {
        let task = self.task.take().ok_or(CastDisplaySlotActorError::Stopped)?;
        task.await
            .map_err(|error| CastDisplaySlotActorError::Join(error.to_string()))
    }
}

impl Drop for CastDisplaySlotActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Orderly lifecycle paths consume the actor through `remove`.
            // Aborting here is the fail-closed fallback: dropping a
            // JoinHandle would orphan the resource-owning task.
            task.abort();
        }
    }
}

#[derive(Debug)]
enum SlotCommand {
    UpdateDevice {
        device: DeviceInfo,
        response: oneshot::Sender<bool>,
    },
    Remove {
        reason: DeviceSessionStopReason,
        response: oneshot::Sender<Result<(), RemoveCastDisplayError>>,
    },
}

async fn run_slot(
    resources: AddedCastDisplayResources,
    mut commands: mpsc::Receiver<SlotCommand>,
    state: watch::Sender<AddedCastDisplaySnapshot>,
    events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
) {
    let AddedCastDisplayResources {
        display_id,
        prepared,
        slot,
        media_driver,
        recovery_factory,
        session_replacement,
        initial_session_generation,
        session_events,
        mut kernel,
        ..
    } = resources;
    let mut device_session = DeviceSessionPolicyState::new(
        prepared.device(),
        state.borrow().device.availability == DeviceAvailability::Available,
        initial_session_generation.get(),
    );
    let mut recovery = DeviceSessionRecoveryActor::spawn(
        recovery_factory,
        session_replacement,
        prepared,
        initial_session_generation,
        session_events,
    )
    .expect("cast-display slot task runs inside Tokio");
    let recovery_handle = recovery.handle();
    let mut media_policy = DisplayMediaPolicyActor::spawn(
        media_driver,
        media_policy_input(&state.borrow(), &device_session),
    )
    .expect("cast-display slot task runs inside Tokio");
    let mut media_state = media_policy.subscribe();
    let mut media_events_open = true;
    let mut removal = None;
    let mut terminal_error = None;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(SlotCommand::UpdateDevice { device, response }) => {
                    let recovery_action = device_session.observe_device(&device);
                    let changed = update_device(&state, device);
                    if changed {
                        publish(&state, &events);
                    }
                    if changed || recovery_action.is_some() {
                        media_policy.observe(media_policy_input(&state.borrow(), &device_session));
                    }
                    let _ = response.send(changed);
                    match recovery_action {
                        Some(DeviceSessionAction::Cancel) => recovery_handle.cancel_phase(),
                        Some(DeviceSessionAction::Recover(device)) => {
                            match recovery_handle.recover(device.clone()).await {
                                Ok(request_generation) => {
                                    device_session.begin_request(request_generation, &device);
                                }
                                Err(error) => {
                                    let diagnostic = format!("start Device-session recovery: {error}");
                                    publish_media_failure(&state, &events, &diagnostic);
                                    terminal_error = Some(diagnostic);
                                    break;
                                }
                            }
                        }
                        None => {}
                    }
                }
                Some(SlotCommand::Remove { reason, response }) => {
                    removal = Some((reason, response));
                    break;
                }
                None => break,
            },
            event = kernel.next_event() => match event {
                Ok(event) => {
                    let revoked = event == KernelDisplayEvent::Revoked;
                    let media_failure = match &event {
                        KernelDisplayEvent::MediaFailed(error) => Some(error.clone()),
                        _ => None,
                    };
                    apply_kernel_event(&state, &events, event);
                    media_policy.observe(media_policy_input(&state.borrow(), &device_session));
                    if let Some(error) = media_failure {
                        let _ = media_policy.report_failure(error).await;
                    }
                    if revoked {
                        terminal_error = Some("CastKMS grant was revoked".into());
                        break;
                    }
                }
                Err(error) => {
                    let diagnostic = error.to_string();
                    state.send_modify(|snapshot| {
                        snapshot.runtime.observe_topology(DisplayTopology {
                            attachment: AttachmentState::Unknown,
                            route: None,
                        });
                        let media_generation = snapshot.runtime.media_generation;
                        snapshot.runtime.observe_media(
                            media_generation,
                            MediaState::Failed,
                            Some(diagnostic.clone()),
                        );
                        snapshot.state_revision = snapshot.runtime.revision;
                    });
                    publish(&state, &events);
                    terminal_error = Some(diagnostic);
                    break;
                }
            },
            event = recovery.next_event() => match event {
                Some(DeviceSessionRecoveryEvent::Ready {
                    request_generation,
                    device,
                    session_generation,
                    retired_session_cleanup_error,
                }) => {
                    if device_session.complete_request(
                        request_generation,
                        &device,
                        session_generation.get(),
                        &state.borrow().device,
                    ) {
                        if let Some(error) = retired_session_cleanup_error {
                            warn!(%display_id, %error, "retired Device session did not acknowledge final cleanup");
                        }
                        media_policy.observe(media_policy_input(&state.borrow(), &device_session));
                    }
                }
                Some(DeviceSessionRecoveryEvent::Failed {
                    request_generation,
                    device,
                    error,
                }) => {
                    if device_session.fail_request(request_generation, &device) {
                        let diagnostic = format!("Device-session recovery failed: {error}");
                        publish_media_failure(&state, &events, &diagnostic);
                        terminal_error = Some(diagnostic);
                        break;
                    }
                }
                Some(DeviceSessionRecoveryEvent::TransportFailed {
                    session_generation,
                    error,
                }) => {
                    if device_session.transport_failed(session_generation.get()) {
                        let diagnostic = format!("Device session transport failed: {error}");
                        publish_media_failure(&state, &events, &diagnostic);
                        media_policy.observe(media_policy_input(&state.borrow(), &device_session));
                        let current = state.borrow().device.clone();
                        if current.availability == DeviceAvailability::Available {
                            match recovery_handle.recover(current.clone()).await {
                                Ok(request_generation) => {
                                    device_session.begin_request(request_generation, &current);
                                }
                                Err(recovery_error) => {
                                    warn!(%display_id, %recovery_error, "could not start recovery after Device-session transport failure");
                                    terminal_error = Some(format!(
                                        "start recovery after Device-session transport failure: {recovery_error}"
                                    ));
                                }
                            }
                        }
                        let _ = media_policy.report_failure(diagnostic).await;
                        if terminal_error.is_some() {
                            break;
                        }
                    }
                }
                None => {
                    let diagnostic = "Device-session recovery coordinator stopped".to_string();
                    publish_media_failure(&state, &events, &diagnostic);
                    terminal_error = Some(diagnostic);
                    break;
                }
            },
            result = media_state.changed(), if media_events_open => {
                if result.is_err() {
                    media_events_open = false;
                    continue;
                }
                let media = media_state.borrow_and_update().clone();
                let changed = state.send_if_modified(|snapshot| {
                    if !snapshot.runtime.observe_media(
                        media.media_generation,
                        media.state,
                        media.last_error.clone(),
                    ) {
                        return false;
                    }
                    snapshot.state_revision = snapshot.runtime.revision;
                    true
                });
                if changed {
                    publish(&state, &events);
                }
            },
            event = media_policy.next_event() => {
                terminal_error = Some(match event {
                    Some(MediaPolicyEvent::RecoveryExhausted { error }) => error,
                    None => "media recovery policy stopped unexpectedly".into(),
                });
                break;
            },
        }
    }

    let reason = removal.as_ref().map_or_else(
        || {
            if terminal_error.is_some() {
                DeviceSessionStopReason::DisplayRemoved
            } else {
                DeviceSessionStopReason::DaemonShutdown
            }
        },
        |(reason, _)| *reason,
    );
    let media_reason = match reason {
        DeviceSessionStopReason::DisplayRemoved => MediaStopReason::DisplayRemoved,
        DeviceSessionStopReason::DaemonShutdown => MediaStopReason::BackendShutdown,
    };
    let recovery_error = recovery
        .shutdown()
        .await
        .err()
        .map(|error| error.to_string());
    let media_error = media_policy
        .shutdown(media_reason)
        .await
        .err()
        .map(|error| error.to_string());
    let detach_error = kernel.detach().await.err().map(|error| error.to_string());
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
    drop(slot);
    if let Some((_, response)) = removal {
        if response.send(result).is_err() {
            warn!(%display_id, "cast-display cleanup completed without a waiter");
        }
    } else if let Some(error) = terminal_error {
        let _ = events.send(CastDisplaySlotEvent::TerminalFailure {
            display_id,
            error,
            cleanup_error,
        });
    } else if let Some(error) = cleanup_error {
        warn!(%display_id, %error, "cast-display owner shutdown did not clean up completely");
    }
}

fn media_policy_input(
    snapshot: &AddedCastDisplaySnapshot,
    device_session: &DeviceSessionPolicyState,
) -> MediaPolicyInput {
    MediaPolicyInput {
        attachment: snapshot.runtime.attachment,
        grant: snapshot.grant_state,
        // A live, authenticated Device session is stronger evidence of
        // reachability than a passive discovery record.  In particular, an
        // mDNS goodbye or expiry must not tear down healthy media.
        device_available: device_session.device_available(&snapshot.device),
        device_session_ready: device_session.ready,
        device_session_generation: device_session.session_generation,
        route: MediaRoute::from_display_state(&snapshot.runtime),
    }
}

#[derive(Debug)]
struct DeviceSessionPolicyState {
    ready: bool,
    bound_connection_generation: u64,
    session_generation: u64,
    pending_request: Option<(u64, u64, u64)>,
}

impl DeviceSessionPolicyState {
    fn new(device: &DeviceInfo, ready: bool, session_generation: u64) -> Self {
        Self {
            ready,
            bound_connection_generation: device.connection_generation,
            session_generation,
            pending_request: None,
        }
    }

    fn observe_device(&mut self, device: &DeviceInfo) -> Option<DeviceSessionAction> {
        // Do not let passive discovery replace a live Device session.  The
        // existing transport reports its own terminal failure; recovery can
        // then use the freshest discovery record.
        if self.ready && self.bound_connection_generation == device.connection_generation {
            return None;
        }
        self.ready = false;
        if device.availability != DeviceAvailability::Available {
            self.pending_request = None;
            return Some(DeviceSessionAction::Cancel);
        }
        Some(DeviceSessionAction::Recover(device.clone()))
    }

    fn device_available(&self, device: &DeviceInfo) -> bool {
        self.ready || device.availability == DeviceAvailability::Available
    }

    fn begin_request(&mut self, request_generation: u64, device: &DeviceInfo) {
        self.pending_request = Some((
            request_generation,
            device.connection_generation,
            device.discovery_generation,
        ));
    }

    fn complete_request(
        &mut self,
        request_generation: u64,
        recovered: &DeviceInfo,
        session_generation: u64,
        current: &DeviceInfo,
    ) -> bool {
        if self.pending_request
            != Some((
                request_generation,
                recovered.connection_generation,
                recovered.discovery_generation,
            ))
            || recovered != current
            || current.availability != DeviceAvailability::Available
        {
            return false;
        }
        self.pending_request = None;
        self.ready = true;
        self.bound_connection_generation = recovered.connection_generation;
        self.session_generation = session_generation;
        true
    }

    fn fail_request(&mut self, request_generation: u64, device: &DeviceInfo) -> bool {
        if self.pending_request
            != Some((
                request_generation,
                device.connection_generation,
                device.discovery_generation,
            ))
        {
            return false;
        }
        self.pending_request = None;
        self.ready = false;
        true
    }

    fn transport_failed(&mut self, session_generation: u64) -> bool {
        if !self.ready || self.session_generation != session_generation {
            return false;
        }
        self.ready = false;
        self.pending_request = None;
        true
    }
}

#[derive(Debug)]
enum DeviceSessionAction {
    Cancel,
    Recover(DeviceInfo),
}

fn publish_media_failure(
    state: &watch::Sender<AddedCastDisplaySnapshot>,
    events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
    diagnostic: &str,
) {
    state.send_modify(|snapshot| {
        let media_generation = snapshot.runtime.media_generation;
        snapshot.runtime.observe_media(
            media_generation,
            MediaState::Failed,
            Some(diagnostic.into()),
        );
        snapshot.state_revision = snapshot.runtime.revision;
    });
    publish(state, events);
}

fn initial_snapshot(resources: &AddedCastDisplayResources) -> AddedCastDisplaySnapshot {
    let observation = resources.kernel.initial_observation();
    let metadata = resources.kernel.metadata();
    let mut runtime = DisplayRuntimeState::attached(resources.state_revision);
    runtime.observe_topology(observation.topology);
    AddedCastDisplaySnapshot {
        display_id: resources.display_id,
        state_revision: runtime.revision,
        device: resources.device.clone(),
        prepared: resources.prepared.clone(),
        output: resources.slot.output().clone(),
        grant_id: metadata.grant_id,
        grant_state: observation.grant_state,
        runtime,
    }
}

fn update_device(state: &watch::Sender<AddedCastDisplaySnapshot>, device: DeviceInfo) -> bool {
    let current = state.borrow().device.clone();
    debug_assert_eq!(current.backend_id, device.backend_id);
    debug_assert_eq!(current.device_id, device.device_id);
    if current == device {
        return false;
    }
    state.send_modify(|snapshot| {
        snapshot.runtime.revision = snapshot
            .runtime
            .revision
            .saturating_add(1)
            .max(device.device_revision);
        snapshot.state_revision = snapshot.runtime.revision;
        snapshot.device = device;
    });
    true
}

fn apply_kernel_event(
    state: &watch::Sender<AddedCastDisplaySnapshot>,
    events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
    event: KernelDisplayEvent,
) {
    state.send_modify(|snapshot| match event {
        KernelDisplayEvent::Changed(observation) => {
            let grant_changed = snapshot.grant_state != observation.grant_state;
            let topology_changed = snapshot.runtime.observe_topology(observation.topology);
            snapshot.grant_state = observation.grant_state;
            if grant_changed && !topology_changed {
                snapshot.runtime.revision = snapshot.runtime.revision.saturating_add(1);
            }
            snapshot.state_revision = snapshot.runtime.revision;
        }
        KernelDisplayEvent::Revoked => {
            snapshot.grant_state = DisplayGrantState::Revoked;
            snapshot.runtime.observe_topology(DisplayTopology {
                attachment: AttachmentState::Detached,
                route: None,
            });
            let media_generation = snapshot.runtime.media_generation;
            snapshot.runtime.observe_media(
                media_generation,
                MediaState::Failed,
                Some("CastKMS grant was revoked".into()),
            );
            snapshot.state_revision = snapshot.runtime.revision;
        }
        KernelDisplayEvent::MediaFailed(error) => {
            let media_generation = snapshot.runtime.media_generation;
            snapshot
                .runtime
                .observe_media(media_generation, MediaState::Failed, Some(error));
            snapshot.state_revision = snapshot.runtime.revision;
        }
    });
    publish(state, events);
}

fn publish(
    state: &watch::Sender<AddedCastDisplaySnapshot>,
    events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
) {
    let snapshot = state.borrow().clone();
    info!(
        event = "cast_display_state",
        display_id = %snapshot.display_id,
        state_revision = snapshot.state_revision,
        route_generation = snapshot.runtime.route_generation,
        attachment = ?snapshot.runtime.attachment,
        grant = ?snapshot.grant_state,
        route = ?snapshot.runtime.route,
        media_generation = snapshot.runtime.media_generation,
        media = ?snapshot.runtime.media,
        last_error = ?snapshot.runtime.last_error,
        "cast-display state changed"
    );
    let _ = events.send(CastDisplaySlotEvent::StateChanged(Box::new(snapshot)));
}

#[derive(Debug, Error)]
pub enum CastDisplaySlotError {
    #[error("CastDisplaySlotActor requires a running Tokio runtime")]
    NoRuntime,
    #[error("cast-display slot actor stopped")]
    Stopped,
}

#[derive(Debug, Error)]
pub enum CastDisplaySlotActorError {
    #[error("cast-display slot actor stopped")]
    Stopped,
    #[error("cast-display slot actor task failed: {0}")]
    Join(String),
    #[error(transparent)]
    Cleanup(#[from] RemoveCastDisplayError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(
        availability: DeviceAvailability,
        connection_generation: u64,
        discovery_generation: u64,
        revision: u64,
    ) -> DeviceInfo {
        DeviceInfo {
            backend_id: "chromiacast".into(),
            device_id: "stable-tv-id".into(),
            display_name: "Living Room TV".into(),
            availability,
            connection_generation,
            discovery_generation,
            device_revision: revision,
            metadata: Vec::new(),
        }
    }

    #[test]
    fn passive_discovery_changes_do_not_replace_a_live_session() {
        let initial = device(DeviceAvailability::Available, 1, 2, 3);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 1);

        let mut renamed = initial.clone();
        renamed.display_name = "Den TV".into();
        renamed.device_revision = 4;
        assert!(state.observe_device(&renamed).is_none());
        assert!(state.ready);

        let unavailable = device(DeviceAvailability::Unavailable, 1, 2, 5);
        assert!(state.observe_device(&unavailable).is_none());
        assert!(state.ready);
        assert!(state.device_available(&unavailable));

        let recovered = device(DeviceAvailability::Available, 1, 3, 6);
        assert!(state.observe_device(&recovered).is_none());
        assert!(state.ready);
        assert_eq!(state.session_generation, 1);
    }

    #[test]
    fn backend_reconnection_replaces_a_live_session() {
        let initial = device(DeviceAvailability::Available, 1, 2, 3);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 1);
        let reconnected = device(DeviceAvailability::Available, 2, 3, 4);

        assert!(matches!(
            state.observe_device(&reconnected),
            Some(DeviceSessionAction::Recover(_))
        ));
        assert!(!state.ready);
    }

    #[test]
    fn discovery_drives_recovery_after_the_live_session_fails() {
        let initial = device(DeviceAvailability::Available, 1, 2, 3);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 1);
        assert!(state.transport_failed(1));

        let unavailable = device(DeviceAvailability::Unavailable, 1, 2, 5);
        assert!(matches!(
            state.observe_device(&unavailable),
            Some(DeviceSessionAction::Cancel)
        ));

        let recovered = device(DeviceAvailability::Available, 2, 3, 6);
        assert!(matches!(
            state.observe_device(&recovered),
            Some(DeviceSessionAction::Recover(_))
        ));
        state.begin_request(7, &recovered);
        assert!(!state.complete_request(6, &recovered, 2, &recovered));
        assert!(state.complete_request(7, &recovered, 2, &recovered));
        assert!(state.ready);
        assert_eq!(state.session_generation, 2);
    }

    #[test]
    fn stale_transport_failure_cannot_disrupt_the_replacement_session() {
        let initial = device(DeviceAvailability::Available, 1, 1, 1);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 4);
        assert!(!state.transport_failed(3));
        assert!(state.ready);
        assert!(state.transport_failed(4));
        assert!(!state.ready);
        assert!(!state.transport_failed(4));
    }
}
