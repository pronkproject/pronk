//! Per-display aggregate actor.
//!
//! The manager owns this aggregate but interacts through its cheap handle. The
//! actor alone mutates the configured Device projection and kernel-derived
//! attachment/route state, and it owns ordered Device-session/kernel teardown.

mod runtime;
use runtime::run_slot;

use pronk_dbus::{DeviceAvailability, DeviceInfo};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::device_session_port::DeviceSessionStopReason;
use crate::display::{
    AddedCastDisplay, AddedCastDisplayResources, AddedCastDisplaySnapshot, CastDisplayId,
    RemoveCastDisplayError,
};
use crate::display_state::{
    AttachmentState, DisplayGrantState, DisplayRuntimeState, DisplayTopology, MediaStatus,
};
use crate::kernel_display_port::KernelDisplayEvent;
use crate::media_policy::{DeviceSessionReadiness, MediaPolicyInput, MediaPolicyTopology};
use crate::media_session::MediaRoute;

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
    owner_dropped: Option<oneshot::Sender<()>>,
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
        let (owner_dropped, owner_drop_signal) = oneshot::channel();
        let (state_tx, state) = watch::channel(initial);
        let task = tokio::spawn(run_slot(
            resources,
            command_rx,
            owner_drop_signal,
            state_tx,
            events,
        ));
        Ok(Self {
            handle: CastDisplaySlotHandle {
                display_id,
                commands,
                state,
            },
            owner_dropped: Some(owner_dropped),
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
    /// Joining here lets the manager reap that completed owner.
    pub(crate) async fn join_after_terminal(mut self) -> Result<(), CastDisplaySlotActorError> {
        let task = self.task.take().ok_or(CastDisplaySlotActorError::Stopped)?;
        task.await
            .map_err(|error| CastDisplaySlotActorError::Join(error.to_string()))
    }
}

impl Drop for CastDisplaySlotActor {
    fn drop(&mut self) {
        self.owner_dropped.take();
        // The task owns the resources until finish completes, including when
        // an in-flight remove call loses its waiter.
        self.task.take();
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

fn media_policy_input(
    snapshot: &AddedCastDisplaySnapshot,
    device_session: &DeviceSessionPolicyState,
) -> MediaPolicyInput {
    MediaPolicyInput {
        topology: match snapshot.runtime.attachment() {
            AttachmentState::Attached => MediaRoute::from_display_state(&snapshot.runtime)
                .map_or(MediaPolicyTopology::Unrouted, MediaPolicyTopology::Routed),
            AttachmentState::Detached => MediaPolicyTopology::Detached,
            AttachmentState::Unknown => MediaPolicyTopology::Unknown,
        },
        grant: snapshot.grant_state,
        // A live, authenticated Device session is stronger evidence of
        // reachability than a passive discovery record.  In particular, an
        // mDNS goodbye or expiry must not tear down healthy media.
        device_session: device_session.readiness(&snapshot.device),
        device_session_generation: device_session.session_generation(),
    }
}

#[derive(Debug)]
enum DeviceSessionPolicyState {
    Ready {
        bound_connection_generation: u64,
        session_generation: u64,
    },
    Recovering {
        request: PendingSessionRequest,
        last_session_generation: u64,
    },
    Unavailable {
        last_session_generation: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingSessionRequest {
    request_generation: u64,
    connection_generation: u64,
    discovery_generation: u64,
    device_revision: u64,
}

impl PendingSessionRequest {
    fn new(request_generation: u64, device: &DeviceInfo) -> Self {
        Self {
            request_generation,
            connection_generation: device.connection_generation,
            discovery_generation: device.discovery_generation,
            device_revision: device.device_revision,
        }
    }
}

impl DeviceSessionPolicyState {
    fn new(device: &DeviceInfo, ready: bool, session_generation: u64) -> Self {
        if ready {
            Self::Ready {
                bound_connection_generation: device.connection_generation,
                session_generation,
            }
        } else {
            Self::Unavailable {
                last_session_generation: session_generation,
            }
        }
    }

    fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    fn session_generation(&self) -> u64 {
        match self {
            Self::Ready {
                session_generation, ..
            } => *session_generation,
            Self::Recovering {
                last_session_generation,
                ..
            }
            | Self::Unavailable {
                last_session_generation,
            } => *last_session_generation,
        }
    }

    fn observe_device(&mut self, device: &DeviceInfo) -> Option<DeviceSessionAction> {
        // Do not let passive discovery replace a live Device session. The
        // existing transport reports its own terminal failure; recovery can
        // then use the freshest discovery record.
        if matches!(self, Self::Ready { bound_connection_generation, .. }
            if *bound_connection_generation == device.connection_generation)
        {
            return None;
        }
        if matches!(self, Self::Recovering { request, .. }
            if device.availability == DeviceAvailability::Available
                && request.connection_generation == device.connection_generation
                && request.discovery_generation == device.discovery_generation
                && request.device_revision == device.device_revision)
        {
            return None;
        }
        *self = Self::Unavailable {
            last_session_generation: self.session_generation(),
        };
        if device.availability != DeviceAvailability::Available {
            return Some(DeviceSessionAction::Cancel);
        }
        Some(DeviceSessionAction::Recover(device.clone()))
    }

    fn readiness(&self, device: &DeviceInfo) -> DeviceSessionReadiness {
        if self.is_ready() {
            DeviceSessionReadiness::Ready
        } else if device.availability == DeviceAvailability::Available {
            DeviceSessionReadiness::Available
        } else {
            DeviceSessionReadiness::Unavailable
        }
    }

    fn begin_request(&mut self, request_generation: u64, device: &DeviceInfo) {
        *self = Self::Recovering {
            request: PendingSessionRequest::new(request_generation, device),
            last_session_generation: self.session_generation(),
        };
    }

    fn complete_request(
        &mut self,
        request_generation: u64,
        recovered: &DeviceInfo,
        session_generation: u64,
        current: &DeviceInfo,
    ) -> bool {
        if !matches!(self, Self::Recovering { request, .. }
            if *request == PendingSessionRequest::new(request_generation, recovered))
            || recovered != current
            || current.availability != DeviceAvailability::Available
        {
            return false;
        }
        *self = Self::Ready {
            bound_connection_generation: recovered.connection_generation,
            session_generation,
        };
        true
    }

    fn fail_request(&mut self, request_generation: u64, device: &DeviceInfo) -> bool {
        if !matches!(self, Self::Recovering { request, .. }
            if *request == PendingSessionRequest::new(request_generation, device))
        {
            return false;
        }
        *self = Self::Unavailable {
            last_session_generation: self.session_generation(),
        };
        true
    }

    fn transport_failed(&mut self, session_generation: u64) -> bool {
        if !matches!(self, Self::Ready { session_generation: current, .. }
            if *current == session_generation)
        {
            return false;
        }
        *self = Self::Unavailable {
            last_session_generation: session_generation,
        };
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
        let media_generation = snapshot.runtime.media_generation();
        snapshot
            .runtime
            .observe_media(media_generation, MediaStatus::Failed(diagnostic.into()));
        snapshot.state_revision = snapshot.runtime.revision();
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
        state_revision: runtime.revision(),
        device: resources.device.clone(),
        prepared: resources.prepared.clone(),
        output: resources.slot.output().clone(),
        kernel_session_id: metadata.session_id,
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
        snapshot
            .runtime
            .advance_for_external_change(device.device_revision);
        snapshot.state_revision = snapshot.runtime.revision();
        snapshot.device = device;
    });
    true
}

fn apply_kernel_event(
    state: &watch::Sender<AddedCastDisplaySnapshot>,
    events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
    event: KernelDisplayEvent,
) {
    let changed = state.send_if_modified(|snapshot| {
        let changed = observe_kernel_event(&mut snapshot.runtime, &mut snapshot.grant_state, event);
        if changed {
            snapshot.state_revision = snapshot.runtime.revision();
        }
        changed
    });
    if changed {
        publish(state, events);
    }
}

fn observe_kernel_event(
    runtime: &mut DisplayRuntimeState,
    grant: &mut DisplayGrantState,
    event: KernelDisplayEvent,
) -> bool {
    match event {
        KernelDisplayEvent::Changed(observation) => {
            let grant_changed = *grant != observation.grant_state;
            let topology_changed = runtime.observe_topology(observation.topology);
            *grant = observation.grant_state;
            if grant_changed && !topology_changed {
                runtime.advance_for_external_change(0);
            }
            grant_changed || topology_changed
        }
        KernelDisplayEvent::Revoked => {
            let grant_changed = *grant != DisplayGrantState::Revoked;
            *grant = DisplayGrantState::Revoked;
            let topology_changed = runtime.observe_topology(DisplayTopology::Detached);
            if grant_changed && !topology_changed {
                runtime.advance_for_external_change(0);
            }
            let media_generation = runtime.media_generation();
            let media_changed = runtime.observe_media(
                media_generation,
                MediaStatus::Failed("CastKMS grant was revoked".into()),
            );
            grant_changed || topology_changed || media_changed
        }
        KernelDisplayEvent::MediaFailed {
            media_generation,
            error,
        } => {
            let current = runtime.media_generation();
            if media_generation.is_none_or(|generation| generation.get() == current) {
                runtime.observe_media(current, MediaStatus::Failed(error))
            } else {
                false
            }
        }
    }
}

fn current_media_failure(current: u64, event: &KernelDisplayEvent) -> Option<String> {
    match event {
        KernelDisplayEvent::MediaFailed {
            media_generation,
            error,
        } if media_generation.is_none_or(|generation| generation.get() == current) => {
            Some(error.clone())
        }
        _ => None,
    }
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
        route_generation = snapshot.runtime.route_generation(),
        attachment = ?snapshot.runtime.attachment(),
        grant = ?snapshot.grant_state,
        route = ?snapshot.runtime.route(),
        media_generation = snapshot.runtime.media_generation(),
        media = ?snapshot.runtime.media(),
        last_error = ?snapshot.runtime.last_error(),
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
    use std::num::NonZeroU64;

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
        assert!(state.is_ready());

        let unavailable = device(DeviceAvailability::Unavailable, 1, 2, 5);
        assert!(state.observe_device(&unavailable).is_none());
        assert!(state.is_ready());
        assert_eq!(state.readiness(&unavailable), DeviceSessionReadiness::Ready);

        let recovered = device(DeviceAvailability::Available, 1, 3, 6);
        assert!(state.observe_device(&recovered).is_none());
        assert!(state.is_ready());
        assert_eq!(state.session_generation(), 1);
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
        assert!(!state.is_ready());
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
        assert!(state.is_ready());
        assert_eq!(state.session_generation(), 2);
    }

    #[test]
    fn withdrawn_device_invalidates_an_in_flight_recovery() {
        let initial = device(DeviceAvailability::Available, 1, 2, 3);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 4);
        let reconnected = device(DeviceAvailability::Available, 2, 3, 4);
        assert!(matches!(
            state.observe_device(&reconnected),
            Some(DeviceSessionAction::Recover(_))
        ));
        state.begin_request(7, &reconnected);

        let unavailable = device(DeviceAvailability::Unavailable, 2, 3, 5);
        assert!(matches!(
            state.observe_device(&unavailable),
            Some(DeviceSessionAction::Cancel)
        ));
        assert!(!state.complete_request(7, &reconnected, 5, &unavailable));
        assert_eq!(state.session_generation(), 4);
        assert!(!state.is_ready());
    }

    #[test]
    fn repeated_device_observation_keeps_the_pending_recovery() {
        let initial = device(DeviceAvailability::Available, 1, 2, 3);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 1);
        let replacement = device(DeviceAvailability::Available, 2, 3, 4);
        assert!(matches!(
            state.observe_device(&replacement),
            Some(DeviceSessionAction::Recover(_))
        ));
        state.begin_request(7, &replacement);

        assert!(state.observe_device(&replacement).is_none());
        assert!(matches!(
            state,
            DeviceSessionPolicyState::Recovering {
                request: PendingSessionRequest {
                    request_generation: 7,
                    ..
                },
                ..
            }
        ));

        let revised = device(DeviceAvailability::Available, 2, 3, 5);
        assert!(matches!(
            state.observe_device(&revised),
            Some(DeviceSessionAction::Recover(_))
        ));
    }

    #[test]
    fn media_failure_only_applies_to_its_current_generation() {
        let current = KernelDisplayEvent::MediaFailed {
            media_generation: NonZeroU64::new(7),
            error: "renderer stopped".into(),
        };
        assert_eq!(
            current_media_failure(7, &current).as_deref(),
            Some("renderer stopped")
        );
        assert_eq!(current_media_failure(8, &current), None);

        let untagged = KernelDisplayEvent::MediaFailed {
            media_generation: None,
            error: "grant failed".into(),
        };
        assert_eq!(
            current_media_failure(8, &untagged).as_deref(),
            Some("grant failed")
        );
    }

    #[test]
    fn repeated_kernel_observations_do_not_advance_display_state() {
        use crate::kernel_display_port::KernelDisplayObservation;

        let mut runtime = DisplayRuntimeState::attached(1);
        let mut grant = DisplayGrantState::Active;
        let observation = KernelDisplayEvent::Changed(KernelDisplayObservation {
            topology: DisplayTopology::Attached { route: None },
            grant_state: DisplayGrantState::Active,
        });
        assert!(!observe_kernel_event(&mut runtime, &mut grant, observation));
        assert_eq!(runtime.revision(), 1);

        assert!(observe_kernel_event(
            &mut runtime,
            &mut grant,
            KernelDisplayEvent::Changed(KernelDisplayObservation {
                topology: DisplayTopology::Attached { route: None },
                grant_state: DisplayGrantState::SuspendedNoMaster,
            }),
        ));
        assert_eq!(runtime.revision(), 2);

        assert!(observe_kernel_event(
            &mut runtime,
            &mut grant,
            KernelDisplayEvent::Revoked,
        ));
        let revision = runtime.revision();
        assert!(!observe_kernel_event(
            &mut runtime,
            &mut grant,
            KernelDisplayEvent::Revoked,
        ));
        assert_eq!(runtime.revision(), revision);
        assert!(!observe_kernel_event(
            &mut runtime,
            &mut grant,
            KernelDisplayEvent::MediaFailed {
                media_generation: None,
                error: "CastKMS grant was revoked".into(),
            },
        ));
        assert_eq!(runtime.revision(), revision);
    }

    #[test]
    fn stale_transport_failure_cannot_disrupt_the_replacement_session() {
        let initial = device(DeviceAvailability::Available, 1, 1, 1);
        let mut state = DeviceSessionPolicyState::new(&initial, true, 4);
        assert!(!state.transport_failed(3));
        assert!(state.is_ready());
        assert!(state.transport_failed(4));
        assert!(!state.is_ready());
        assert!(!state.transport_failed(4));
    }
}
