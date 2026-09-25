//! One manager-independent media-session actor.
//!
//! The actor owns transition ordering and cancellation. Concrete capture,
//! PipeWire, and backend adapters implement [`MediaSessionDriver`]; the state
//! machine never depends on those infrastructure layers.

mod runtime;
use runtime::ActorRuntime;

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::display_state::{DisplayRuntimeState, MediaState, RouteState, RouteTarget, RoutedMode};

// Bounds queued control work to a small, explicit amount while leaving room
// for concurrent policy, route, and user-control edges.
const COMMAND_CAPACITY: usize = 32;
const MAX_ERROR_BYTES: usize = 512;
pub const DEFAULT_MEDIA_PHASE_TIMEOUT: Duration = Duration::from_secs(15);
// Display removal and daemon exit must relinquish local owners promptly. This
// is a hard ceiling for their complete media cleanup, independent of the
// longer budget used while serving an active display.
pub const MAX_MEDIA_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaRoute {
    /// Slot-owned generation advanced for every materially different route.
    pub route_generation: u64,
    /// Opaque capture target selected by the kernel-display adapter.
    pub target: RouteTarget,
    pub mode: RoutedMode,
}

impl MediaRoute {
    pub fn from_display_state(state: &DisplayRuntimeState) -> Option<Self> {
        let RouteState::Active(route) = state.route() else {
            return None;
        };
        (state.route_generation() != 0).then_some(Self {
            route_generation: state.route_generation(),
            target: route.target,
            mode: route.mode,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaStartRequest {
    pub media_generation: u64,
    pub route: MediaRoute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSuspendReason {
    GrantUnavailable,
    DeviceUnavailable,
    SessionInactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaStopReason {
    OutputDisabled,
    ModeChanged,
    DisplayRemoved,
    BackendShutdown,
    TransportFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSessionSnapshot {
    revision: u64,
    media_generation: u64,
    phase: MediaPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MediaPhase {
    Idle,
    StartingCapture(MediaRoute),
    StartingMedia(MediaRoute),
    Running(MediaRoute),
    Suspended(MediaRoute),
    Reconfiguring(MediaRoute),
    Stopping,
    Failed {
        route: Option<MediaRoute>,
        error: String,
    },
}

impl MediaSessionSnapshot {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn media_generation(&self) -> u64 {
        self.media_generation
    }
    fn idle() -> Self {
        Self {
            revision: 1,
            media_generation: 0,
            phase: MediaPhase::Idle,
        }
    }

    pub fn state(&self) -> MediaState {
        match self.phase {
            MediaPhase::Idle => MediaState::Idle,
            MediaPhase::StartingCapture(_) => MediaState::StartingCapture,
            MediaPhase::StartingMedia(_) => MediaState::StartingMedia,
            MediaPhase::Running(_) => MediaState::Running,
            MediaPhase::Suspended(_) => MediaState::Suspended,
            MediaPhase::Reconfiguring(_) => MediaState::Reconfiguring,
            MediaPhase::Stopping => MediaState::Stopping,
            MediaPhase::Failed { .. } => MediaState::Failed,
        }
    }

    pub fn route(&self) -> Option<MediaRoute> {
        match self.phase {
            MediaPhase::StartingCapture(route)
            | MediaPhase::StartingMedia(route)
            | MediaPhase::Running(route)
            | MediaPhase::Suspended(route)
            | MediaPhase::Reconfiguring(route) => Some(route),
            MediaPhase::Failed { route, .. } => route,
            MediaPhase::Idle | MediaPhase::Stopping => None,
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        match &self.phase {
            MediaPhase::Failed { error, .. } => Some(error),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_snapshot(state: MediaState, route: Option<MediaRoute>) -> Self {
        let phase = match (state, route) {
            (MediaState::Idle, None) => MediaPhase::Idle,
            (MediaState::Running, Some(route)) => MediaPhase::Running(route),
            (MediaState::Failed, route) => MediaPhase::Failed {
                route,
                error: "test failure".into(),
            },
            _ => panic!("invalid test media phase"),
        };
        Self {
            revision: 1,
            media_generation: u64::from(route.is_some()),
            phase,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct MediaDriverError(String);

impl MediaDriverError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(bounded_text(message.into(), MAX_ERROR_BYTES))
    }
}

/// Infrastructure port consumed by the media-session use case.
///
/// Implementations own their capture/PipeWire/backend resources. Every phase
/// must honor cancellation and leave enough state for `stop` to be idempotent.
#[async_trait]
pub trait MediaSessionDriver: fmt::Debug + Send + 'static {
    async fn start_capture(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError>;

    async fn start_media(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError>;

    async fn suspend(
        &mut self,
        media_generation: u64,
        reason: MediaSuspendReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError>;

    async fn stop(
        &mut self,
        media_generation: u64,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError>;

    /// Final resource-owner shutdown. This is called exactly once even when
    /// no media generation was ever started.
    async fn shutdown(
        &mut self,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError>;
}

#[derive(Debug, Clone, Copy)]
pub struct MediaSessionPolicy {
    pub phase_timeout: Duration,
}

impl Default for MediaSessionPolicy {
    fn default() -> Self {
        Self {
            phase_timeout: DEFAULT_MEDIA_PHASE_TIMEOUT,
        }
    }
}

pub struct MediaSessionActor {
    handle: MediaSessionHandle,
    shutdown_cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct RequestCoordinator {
    state: Mutex<RequestState>,
}

#[derive(Debug)]
struct RequestState {
    installed_phase: CancellationToken,
    latest_generation: u64,
}

impl RequestCoordinator {
    fn new() -> Self {
        Self {
            state: Mutex::new(RequestState {
                installed_phase: CancellationToken::new(),
                latest_generation: 0,
            }),
        }
    }

    fn begin_request(&self) -> Result<u64, MediaSessionActorError> {
        let mut state = self
            .state
            .lock()
            .expect("media request coordinator mutex poisoned");
        let generation = state
            .latest_generation
            .checked_add(1)
            .ok_or(MediaSessionActorError::ControlGenerationExhausted)?;
        state.installed_phase.cancel();
        state.latest_generation = generation;
        Ok(generation)
    }

    fn interrupt_phase(&self) {
        self.state
            .lock()
            .expect("media request coordinator mutex poisoned")
            .installed_phase
            .cancel();
    }

    fn install_phase(&self, request_generation: u64) -> CancellationToken {
        let token = CancellationToken::new();
        let mut state = self
            .state
            .lock()
            .expect("media request coordinator mutex poisoned");
        state.installed_phase.cancel();
        state.installed_phase = token.clone();
        if state.latest_generation != request_generation {
            // A newer command was queued before this phase was installed. Without
            // this check, that command's earlier interrupt would be lost and the
            // stale phase could block the queue for a full timeout.
            token.cancel();
        }
        token
    }

    fn is_current(&self, request_generation: u64) -> bool {
        self.state
            .lock()
            .expect("media request coordinator mutex poisoned")
            .latest_generation
            == request_generation
    }
}

#[derive(Debug)]
struct ActorCancellation {
    requests: Arc<RequestCoordinator>,
    shutdown: CancellationToken,
}

impl ActorCancellation {
    fn cleanup_phase(&self) -> CancellationToken {
        self.shutdown.child_token()
    }
}

impl fmt::Debug for MediaSessionActor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaSessionActor")
            .field("snapshot", &self.handle.snapshot())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct MediaSessionHandle {
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<MediaSessionSnapshot>,
    requests: Arc<RequestCoordinator>,
}

impl MediaSessionHandle {
    pub fn snapshot(&self) -> MediaSessionSnapshot {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<MediaSessionSnapshot> {
        self.state.clone()
    }

    pub async fn activate(&self, route: MediaRoute) -> Result<(), MediaSessionActorError> {
        self.request(|response| CommandKind::Activate { route, response })
            .await
    }

    pub async fn deactivate(&self) -> Result<(), MediaSessionActorError> {
        self.request(|response| CommandKind::Deactivate { response })
            .await
    }

    pub async fn suspend(&self, reason: MediaSuspendReason) -> Result<(), MediaSessionActorError> {
        self.request(|response| CommandKind::Suspend { reason, response })
            .await
    }

    pub async fn retry(&self) -> Result<(), MediaSessionActorError> {
        self.request(|response| CommandKind::Retry { response })
            .await
    }

    pub(crate) async fn report_failure(
        &self,
        error: impl Into<String>,
    ) -> Result<(), MediaSessionActorError> {
        self.request(|response| CommandKind::ReportFailure {
            error: bounded_text(error.into(), MAX_ERROR_BYTES),
            response,
        })
        .await
    }

    pub(crate) fn cancel_phase(&self) {
        self.interrupt_phase();
    }

    async fn request(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), MediaSessionActorError>>) -> CommandKind,
    ) -> Result<(), MediaSessionActorError> {
        let request_generation = self.requests.begin_request()?;
        let (response, reply) = oneshot::channel();
        self.commands
            .send(Command {
                request_generation,
                kind: make(response),
            })
            .await
            .map_err(|_| MediaSessionActorError::Stopped)?;
        reply.await.map_err(|_| MediaSessionActorError::Stopped)?
    }

    fn interrupt_phase(&self) {
        self.requests.interrupt_phase();
    }
}

impl MediaSessionActor {
    pub fn spawn(driver: Box<dyn MediaSessionDriver>) -> Result<Self, MediaSessionActorError> {
        Self::spawn_with_policy(driver, MediaSessionPolicy::default())
    }

    pub fn spawn_with_policy(
        driver: Box<dyn MediaSessionDriver>,
        policy: MediaSessionPolicy,
    ) -> Result<Self, MediaSessionActorError> {
        tokio::runtime::Handle::try_current().map_err(|_| MediaSessionActorError::NoRuntime)?;
        if policy.phase_timeout.is_zero() {
            return Err(MediaSessionActorError::InvalidPolicy);
        }
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (state_tx, state) = watch::channel(MediaSessionSnapshot::idle());
        let requests = Arc::new(RequestCoordinator::new());
        let shutdown_cancellation = CancellationToken::new();
        let actor_cancellation = ActorCancellation {
            requests: Arc::clone(&requests),
            shutdown: shutdown_cancellation.clone(),
        };
        let task = tokio::spawn(
            ActorRuntime::new(state_tx, actor_cancellation, driver, policy).run(command_rx),
        );
        Ok(Self {
            handle: MediaSessionHandle {
                commands,
                state,
                requests,
            },
            shutdown_cancellation,
            task: Some(task),
        })
    }

    pub fn handle(&self) -> MediaSessionHandle {
        self.handle.clone()
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutdown_cancellation.cancel();
        self.handle.interrupt_phase();
    }

    pub async fn shutdown(mut self, reason: MediaStopReason) -> Result<(), MediaSessionActorError> {
        self.begin_shutdown();
        let result = self
            .handle
            .request(|response| CommandKind::Shutdown { reason, response })
            .await;
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| MediaSessionActorError::Join(error.to_string()))?;
        }
        result
    }
}

impl Drop for MediaSessionActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.shutdown_cancellation.cancel();
            self.handle.interrupt_phase();
            // `shutdown` is the orderly path. Never orphan a resource-owning
            // actor merely because its owner was dropped unexpectedly.
            task.abort();
        }
    }
}

#[derive(Debug)]
struct Command {
    request_generation: u64,
    kind: CommandKind,
}

#[derive(Debug)]
enum CommandKind {
    Activate {
        route: MediaRoute,
        response: oneshot::Sender<Result<(), MediaSessionActorError>>,
    },
    Deactivate {
        response: oneshot::Sender<Result<(), MediaSessionActorError>>,
    },
    Suspend {
        reason: MediaSuspendReason,
        response: oneshot::Sender<Result<(), MediaSessionActorError>>,
    },
    Retry {
        response: oneshot::Sender<Result<(), MediaSessionActorError>>,
    },
    ReportFailure {
        error: String,
        response: oneshot::Sender<Result<(), MediaSessionActorError>>,
    },
    Shutdown {
        reason: MediaStopReason,
        response: oneshot::Sender<Result<(), MediaSessionActorError>>,
    },
}

impl CommandKind {
    fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown { .. })
    }

    fn reject_superseded(self) {
        let response = match self {
            Self::Activate { response, .. }
            | Self::Deactivate { response }
            | Self::Suspend { response, .. }
            | Self::Retry { response }
            | Self::ReportFailure { response, .. }
            | Self::Shutdown { response, .. } => response,
        };
        let _ = response.send(Err(MediaSessionActorError::Superseded));
    }
}

fn bounded_text(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MediaSessionActorError {
    #[error("MediaSessionActor requires a running Tokio runtime")]
    NoRuntime,
    #[error("media-session policy has a zero phase timeout")]
    InvalidPolicy,
    #[error("media-session actor stopped")]
    Stopped,
    #[error("media-session actor task failed: {0}")]
    Join(String),
    #[error("route generation must be nonzero")]
    InvalidRouteGeneration,
    #[error("media generation is exhausted")]
    GenerationExhausted,
    #[error("media control-request generation is exhausted")]
    ControlGenerationExhausted,
    #[error("media control request was superseded")]
    Superseded,
    #[error("cannot suspend media from {0:?}")]
    SuspendUnavailable(MediaState),
    #[error("cannot retry media from {0:?}")]
    RetryUnavailable(MediaState),
    #[error("media phase {phase} timed out after {timeout:?}")]
    PhaseTimeout {
        phase: &'static str,
        timeout: Duration,
    },
    #[error("media phase {phase} failed: {source}")]
    Driver {
        phase: &'static str,
        source: MediaDriverError,
    },
    #[error("media cleanup failed: {media}; final driver shutdown also failed: {final_error}")]
    CombinedShutdown { media: String, final_error: String },
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Capture(u64),
        Media(u64),
        Suspend(u64, MediaSuspendReason),
        Stop(u64, MediaStopReason),
        Shutdown(MediaStopReason),
    }

    #[derive(Debug, Clone, Default)]
    struct FakeDriver {
        calls: Arc<Mutex<Vec<Call>>>,
        fail_media_once: Arc<AtomicBool>,
        block_capture: Arc<AtomicBool>,
        block_stop: Arc<AtomicBool>,
    }

    impl FakeDriver {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MediaSessionDriver for FakeDriver {
        async fn start_capture(
            &mut self,
            request: MediaStartRequest,
            cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Capture(request.media_generation));
            if self.block_capture.load(Ordering::SeqCst) {
                cancellation.cancelled().await;
                return Err(MediaDriverError::new("capture cancelled"));
            }
            Ok(())
        }

        async fn start_media(
            &mut self,
            request: MediaStartRequest,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Media(request.media_generation));
            if self.fail_media_once.swap(false, Ordering::SeqCst) {
                Err(MediaDriverError::new("encoder refused generation"))
            } else {
                Ok(())
            }
        }

        async fn suspend(
            &mut self,
            media_generation: u64,
            reason: MediaSuspendReason,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Suspend(media_generation, reason));
            Ok(())
        }

        async fn stop(
            &mut self,
            media_generation: u64,
            reason: MediaStopReason,
            cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Stop(media_generation, reason));
            if self.block_stop.load(Ordering::SeqCst) {
                cancellation.cancelled().await;
                return Err(MediaDriverError::new("stop cancelled"));
            }
            Ok(())
        }

        async fn shutdown(
            &mut self,
            reason: MediaStopReason,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls.lock().unwrap().push(Call::Shutdown(reason));
            Ok(())
        }
    }

    fn route(generation: u64, width: u32) -> MediaRoute {
        MediaRoute {
            route_generation: generation,
            target: RouteTarget::new(std::num::NonZeroU32::new(7).unwrap()),
            mode: RoutedMode {
                width,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            },
        }
    }

    #[test]
    fn active_display_state_becomes_a_generation_bound_media_route() {
        use crate::display_state::{ActiveRoute, DisplayTopology};

        let mut state = DisplayRuntimeState::attached(1);
        assert_eq!(MediaRoute::from_display_state(&state), None);
        state.observe_topology(DisplayTopology::Attached {
            route: Some(ActiveRoute {
                target: RouteTarget::new(std::num::NonZeroU32::new(7).unwrap()),
                mode: route(1, 1920).mode,
            }),
        });

        assert_eq!(MediaRoute::from_display_state(&state), Some(route(1, 1920)));
    }

    #[tokio::test]
    async fn active_route_runs_ordered_phases_and_disable_retains_the_actor() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();

        handle.activate(route(1, 1920)).await.unwrap();
        assert_eq!(handle.snapshot().state(), MediaState::Running);
        assert_eq!(handle.snapshot().media_generation, 1);
        handle.deactivate().await.unwrap();
        assert_eq!(handle.snapshot().state(), MediaState::Idle);
        assert_eq!(
            driver.calls(),
            vec![
                Call::Capture(1),
                Call::Media(1),
                Call::Stop(1, MediaStopReason::OutputDisabled),
            ]
        );

        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
        assert_eq!(
            driver.calls().last(),
            Some(&Call::Shutdown(MediaStopReason::BackendShutdown))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_queued_stale_command_cannot_undo_a_newer_route() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();
        let stale_generation = handle.requests.begin_request().unwrap();
        let current_generation = handle.requests.begin_request().unwrap();
        let (current_response, current_reply) = oneshot::channel();
        let (stale_response, stale_reply) = oneshot::channel();
        handle
            .commands
            .try_send(Command {
                request_generation: current_generation,
                kind: CommandKind::Activate {
                    route: route(1, 1920),
                    response: current_response,
                },
            })
            .unwrap();
        handle
            .commands
            .try_send(Command {
                request_generation: stale_generation,
                kind: CommandKind::Deactivate {
                    response: stale_response,
                },
            })
            .unwrap();

        current_reply.await.unwrap().unwrap();
        assert_eq!(
            stale_reply.await.unwrap(),
            Err(MediaSessionActorError::Superseded)
        );
        assert_eq!(handle.snapshot().state(), MediaState::Running);
        assert_eq!(driver.calls(), vec![Call::Capture(1), Call::Media(1)]);
        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mode_change_stops_before_starting_a_new_generation() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();
        handle.activate(route(1, 1920)).await.unwrap();
        handle.activate(route(2, 1280)).await.unwrap();

        assert_eq!(handle.snapshot().state(), MediaState::Running);
        assert_eq!(handle.snapshot().media_generation, 2);
        assert_eq!(handle.snapshot().route(), Some(route(2, 1280)));
        assert_eq!(
            driver.calls(),
            vec![
                Call::Capture(1),
                Call::Media(1),
                Call::Stop(1, MediaStopReason::ModeChanged),
                Call::Capture(2),
                Call::Media(2),
            ]
        );

        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_start_rolls_back_and_retry_uses_a_fresh_generation() {
        let driver = FakeDriver {
            fail_media_once: Arc::new(AtomicBool::new(true)),
            ..FakeDriver::default()
        };
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();

        assert!(matches!(
            handle.activate(route(1, 1920)).await,
            Err(MediaSessionActorError::Driver {
                phase: "start backend media",
                ..
            })
        ));
        assert_eq!(handle.snapshot().state(), MediaState::Failed);
        assert!(handle
            .snapshot()
            .last_error()
            .unwrap()
            .contains("encoder refused"));

        handle.retry().await.unwrap();
        assert_eq!(handle.snapshot().state(), MediaState::Running);
        assert_eq!(handle.snapshot().media_generation, 2);
        assert_eq!(
            driver.calls(),
            vec![
                Call::Capture(1),
                Call::Media(1),
                Call::Stop(1, MediaStopReason::TransportFailure),
                Call::Stop(1, MediaStopReason::TransportFailure),
                Call::Capture(2),
                Call::Media(2),
            ]
        );

        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_new_control_command_cancels_a_blocked_phase() {
        let driver = FakeDriver {
            block_capture: Arc::new(AtomicBool::new(true)),
            ..FakeDriver::default()
        };
        let actor = MediaSessionActor::spawn_with_policy(
            Box::new(driver),
            MediaSessionPolicy {
                phase_timeout: Duration::from_secs(2),
            },
        )
        .unwrap();
        let handle = actor.handle();
        let activating = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.activate(route(1, 1920)).await })
        };
        while handle.snapshot().state() != MediaState::StartingCapture {
            tokio::task::yield_now().await;
        }

        let deactivating = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.deactivate().await })
        };
        assert!(matches!(
            activating.await.unwrap(),
            Err(MediaSessionActorError::Driver {
                phase: "start capture",
                ..
            })
        ));
        deactivating.await.unwrap().unwrap();
        assert_eq!(handle.snapshot().state(), MediaState::Idle);

        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn suspension_preserves_route_and_generation() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();
        handle.activate(route(1, 1920)).await.unwrap();
        handle
            .suspend(MediaSuspendReason::GrantUnavailable)
            .await
            .unwrap();
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.state(), MediaState::Suspended);
        assert_eq!(snapshot.media_generation, 1);
        assert_eq!(snapshot.route(), Some(route(1, 1920)));
        assert_eq!(
            driver.calls().last(),
            Some(&Call::Suspend(1, MediaSuspendReason::GrantUnavailable))
        );

        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn idle_shutdown_still_releases_the_resource_owning_driver() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap();
        assert_eq!(
            driver.calls(),
            vec![Call::Shutdown(MediaStopReason::BackendShutdown)]
        );
    }

    #[test]
    fn a_queued_newer_request_pre_cancels_an_older_phase() {
        let requests = RequestCoordinator::new();
        assert_eq!(requests.begin_request().unwrap(), 1);
        assert_eq!(requests.begin_request().unwrap(), 2);
        let stale = requests.install_phase(1);
        assert!(stale.is_cancelled());
    }

    #[tokio::test]
    async fn shutdown_shares_one_deadline_and_still_calls_final_owner_cleanup() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn_with_policy(
            Box::new(driver.clone()),
            MediaSessionPolicy {
                phase_timeout: Duration::from_millis(90),
            },
        )
        .unwrap();
        let handle = actor.handle();
        handle.activate(route(1, 1920)).await.unwrap();
        driver.block_stop.store(true, Ordering::SeqCst);

        let started = std::time::Instant::now();
        let error = actor
            .shutdown(MediaStopReason::BackendShutdown)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            MediaSessionActorError::PhaseTimeout {
                phase: "stop media",
                ..
            }
        ));
        assert!(started.elapsed() < Duration::from_millis(150));
        assert_eq!(
            driver.calls().last(),
            Some(&Call::Shutdown(MediaStopReason::BackendShutdown))
        );
    }

    #[tokio::test]
    async fn default_shutdown_is_never_given_the_active_media_phase_budget() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();
        handle.activate(route(1, 1920)).await.unwrap();
        driver.block_stop.store(true, Ordering::SeqCst);

        let started = std::time::Instant::now();
        let error = actor
            .shutdown(MediaStopReason::DisplayRemoved)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            MediaSessionActorError::PhaseTimeout {
                phase: "stop media",
                ..
            }
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(
            driver.calls().last(),
            Some(&Call::Shutdown(MediaStopReason::DisplayRemoved))
        );
    }

    #[tokio::test]
    async fn shutdown_interrupts_an_already_running_cleanup_phase() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn_with_policy(
            Box::new(driver.clone()),
            MediaSessionPolicy {
                phase_timeout: Duration::from_millis(90),
            },
        )
        .unwrap();
        let handle = actor.handle();
        handle.activate(route(1, 1920)).await.unwrap();
        driver.block_stop.store(true, Ordering::SeqCst);

        let deactivating = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.deactivate().await })
        };
        while handle.snapshot().state() != MediaState::Stopping {
            tokio::task::yield_now().await;
        }

        let started = std::time::Instant::now();
        let result = actor.shutdown(MediaStopReason::BackendShutdown).await;
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(result.is_err());
        assert!(deactivating.await.unwrap().is_err());
        assert_eq!(
            driver.calls().last(),
            Some(&Call::Shutdown(MediaStopReason::BackendShutdown))
        );
    }
}
