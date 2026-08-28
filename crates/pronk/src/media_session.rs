//! One manager-independent media-session actor.
//!
//! The actor owns transition ordering and cancellation. Concrete capture,
//! PipeWire, and backend adapters implement [`MediaSessionDriver`]; the state
//! machine never depends on those infrastructure layers.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};
use tokio_util::sync::CancellationToken;

use crate::display_state::{DisplayRuntimeState, MediaState, RouteState, RouteTarget, RoutedMode};

// Bounds queued control work to a small, explicit amount while leaving room
// for concurrent policy, route, and user-control edges.
const COMMAND_CAPACITY: usize = 32;
const MAX_ERROR_BYTES: usize = 512;
pub const DEFAULT_MEDIA_PHASE_TIMEOUT: Duration = Duration::from_secs(15);

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
        let RouteState::Active(route) = state.route else {
            return None;
        };
        (state.route_generation != 0).then_some(Self {
            route_generation: state.route_generation,
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
    pub revision: u64,
    pub media_generation: u64,
    pub state: MediaState,
    pub route: Option<MediaRoute>,
    pub last_error: Option<String>,
}

impl MediaSessionSnapshot {
    fn idle() -> Self {
        Self {
            revision: 1,
            media_generation: 0,
            state: MediaState::Idle,
            route: None,
            last_error: None,
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
    installed_phase: Mutex<CancellationToken>,
    latest_generation: AtomicU64,
}

impl RequestCoordinator {
    fn new() -> Self {
        Self {
            installed_phase: Mutex::new(CancellationToken::new()),
            latest_generation: AtomicU64::new(0),
        }
    }

    fn next_generation(&self) -> Result<u64, MediaSessionActorError> {
        self.latest_generation
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| MediaSessionActorError::ControlGenerationExhausted)?
            .checked_add(1)
            .ok_or(MediaSessionActorError::ControlGenerationExhausted)
    }

    fn interrupt_phase(&self) {
        self.installed_phase
            .lock()
            .expect("media phase cancellation mutex poisoned")
            .cancel();
    }

    fn install_phase(&self, request_generation: u64) -> CancellationToken {
        let token = CancellationToken::new();
        let mut current = self
            .installed_phase
            .lock()
            .expect("media phase cancellation mutex poisoned");
        current.cancel();
        *current = token.clone();
        if self.latest_generation.load(Ordering::SeqCst) != request_generation {
            // A newer command was queued before this phase was installed. Without
            // this check, that command's earlier interrupt would be lost and the
            // stale phase could block the queue for a full timeout.
            token.cancel();
        }
        token
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
        // Advance the request identity before cancelling the installed phase.
        // If the actor installs an older queued request concurrently, it will
        // either observe this newer generation or be cancelled under the same
        // mutex immediately afterward.
        let request_generation = self.requests.next_generation()?;
        self.interrupt_phase();
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
        let task = tokio::spawn(run_actor(
            command_rx,
            state_tx,
            actor_cancellation,
            driver,
            policy,
        ));
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

async fn run_actor(
    mut commands: mpsc::Receiver<Command>,
    state: watch::Sender<MediaSessionSnapshot>,
    cancellation: ActorCancellation,
    mut driver: Box<dyn MediaSessionDriver>,
    policy: MediaSessionPolicy,
) {
    while let Some(command) = commands.recv().await {
        let Command {
            request_generation,
            kind,
        } = command;
        match kind {
            CommandKind::Activate { route, response } => {
                let result = activate(
                    &state,
                    &cancellation,
                    request_generation,
                    driver.as_mut(),
                    policy,
                    route,
                )
                .await;
                let _ = response.send(result);
            }
            CommandKind::Deactivate { response } => {
                let result = stop_to_idle(
                    &state,
                    driver.as_mut(),
                    policy,
                    MediaStopReason::OutputDisabled,
                    cancellation.cleanup_phase(),
                )
                .await;
                let _ = response.send(result);
            }
            CommandKind::Suspend { reason, response } => {
                let result = suspend(
                    &state,
                    &cancellation,
                    request_generation,
                    driver.as_mut(),
                    policy,
                    reason,
                )
                .await;
                let _ = response.send(result);
            }
            CommandKind::Retry { response } => {
                let snapshot = state.borrow().clone();
                let result = match (snapshot.state, snapshot.route) {
                    (MediaState::Failed, Some(route)) => {
                        activate(
                            &state,
                            &cancellation,
                            request_generation,
                            driver.as_mut(),
                            policy,
                            route,
                        )
                        .await
                    }
                    _ => Err(MediaSessionActorError::RetryUnavailable(snapshot.state)),
                };
                let _ = response.send(result);
            }
            CommandKind::ReportFailure { error, response } => {
                let result =
                    report_external_failure(&state, driver.as_mut(), policy, error, &cancellation)
                        .await;
                let _ = response.send(result);
            }
            CommandKind::Shutdown { reason, response } => {
                let result = shutdown_driver(&state, driver.as_mut(), policy, reason).await;
                let _ = response.send(result);
                return;
            }
        }
    }

    let _ = shutdown_driver(
        &state,
        driver.as_mut(),
        policy,
        MediaStopReason::BackendShutdown,
    )
    .await;
}

async fn report_external_failure(
    state: &watch::Sender<MediaSessionSnapshot>,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    error: String,
    cancellation: &ActorCancellation,
) -> Result<(), MediaSessionActorError> {
    let snapshot = state.borrow().clone();
    let cleanup = run_stop(
        state,
        driver,
        policy,
        MediaStopReason::TransportFailure,
        cancellation.cleanup_phase(),
    )
    .await;
    let diagnostic = match &cleanup {
        Ok(()) => error,
        Err(cleanup) => bounded_text(
            format!("{error}; cleanup also failed: {cleanup}"),
            MAX_ERROR_BYTES,
        ),
    };
    set_state(
        state,
        MediaState::Failed,
        snapshot.route,
        None,
        Some(diagnostic),
    );
    cleanup
}

async fn shutdown_driver(
    state: &watch::Sender<MediaSessionSnapshot>,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    reason: MediaStopReason,
) -> Result<(), MediaSessionActorError> {
    // Cleanup and final owner shutdown share one phase budget. Reserving one
    // third for the final call prevents a slow StopMedia from consuming the
    // entire deadline and skipping the resource owner's shutdown hook.
    let deadline = Instant::now() + policy.phase_timeout;
    let final_reserve = (policy.phase_timeout / 3).max(Duration::from_nanos(1));
    let stop_policy = MediaSessionPolicy {
        phase_timeout: policy.phase_timeout.saturating_sub(final_reserve),
    };
    let media_result = stop_to_idle(state, driver, stop_policy, reason, cleanup_phase()).await;
    let final_policy = MediaSessionPolicy {
        phase_timeout: deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_nanos(1)),
    };
    let cancellation = cleanup_phase();
    let final_result = run_phase(
        final_policy,
        "shut down media driver",
        cancellation.clone(),
        driver.shutdown(reason, cancellation),
    )
    .await;
    match (media_result, final_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(media), Err(final_error)) => Err(MediaSessionActorError::CombinedShutdown {
            media: media.to_string(),
            final_error: final_error.to_string(),
        }),
    }
}

async fn activate(
    state: &watch::Sender<MediaSessionSnapshot>,
    cancellation: &ActorCancellation,
    request_generation: u64,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    route: MediaRoute,
) -> Result<(), MediaSessionActorError> {
    if route.route_generation == 0 {
        return Err(MediaSessionActorError::InvalidRouteGeneration);
    }
    let current = state.borrow().clone();
    if current.state == MediaState::Running && current.route == Some(route) {
        return Ok(());
    }
    if current.state == MediaState::Failed {
        // A failed phase may have left protocol authority ambiguous even when
        // its first rollback also failed.  Retry the generation-matched
        // idempotent cleanup before minting a fresh media generation.
        set_state(state, MediaState::Reconfiguring, Some(route), None, None);
        if let Err(error) = run_stop(
            state,
            driver,
            policy,
            MediaStopReason::TransportFailure,
            cancellation.cleanup_phase(),
        )
        .await
        {
            fail(state, Some(route), &error);
            return Err(error);
        }
    } else if current.state != MediaState::Idle {
        set_state(state, MediaState::Reconfiguring, Some(route), None, None);
        if let Err(error) = run_stop(
            state,
            driver,
            policy,
            MediaStopReason::ModeChanged,
            cancellation.cleanup_phase(),
        )
        .await
        {
            fail(state, Some(route), &error);
            return Err(error);
        }
    }

    let generation = state
        .borrow()
        .media_generation
        .checked_add(1)
        .ok_or(MediaSessionActorError::GenerationExhausted)?;
    let request = MediaStartRequest {
        media_generation: generation,
        route,
    };
    set_state(
        state,
        MediaState::StartingCapture,
        Some(route),
        Some(generation),
        None,
    );
    let phase_cancellation = cancellation.requests.install_phase(request_generation);
    if let Err(error) = run_phase(
        policy,
        "start capture",
        phase_cancellation.clone(),
        driver.start_capture(request, phase_cancellation),
    )
    .await
    {
        rollback_after_start_failure(state, driver, policy, request, &error, cancellation).await;
        return Err(error);
    }

    set_state(state, MediaState::StartingMedia, Some(route), None, None);
    let phase_cancellation = cancellation.requests.install_phase(request_generation);
    if let Err(error) = run_phase(
        policy,
        "start backend media",
        phase_cancellation.clone(),
        driver.start_media(request, phase_cancellation),
    )
    .await
    {
        rollback_after_start_failure(state, driver, policy, request, &error, cancellation).await;
        return Err(error);
    }

    set_state(state, MediaState::Running, Some(route), None, None);
    Ok(())
}

async fn suspend(
    state: &watch::Sender<MediaSessionSnapshot>,
    cancellation: &ActorCancellation,
    request_generation: u64,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    reason: MediaSuspendReason,
) -> Result<(), MediaSessionActorError> {
    let snapshot = state.borrow().clone();
    if snapshot.state == MediaState::Suspended {
        return Ok(());
    }
    if snapshot.state != MediaState::Running {
        return Err(MediaSessionActorError::SuspendUnavailable(snapshot.state));
    }
    let phase_cancellation = cancellation.requests.install_phase(request_generation);
    let result = run_phase(
        policy,
        "suspend media",
        phase_cancellation.clone(),
        driver.suspend(snapshot.media_generation, reason, phase_cancellation),
    )
    .await;
    match result {
        Ok(()) => {
            set_state(state, MediaState::Suspended, snapshot.route, None, None);
            Ok(())
        }
        Err(error) => {
            fail(state, snapshot.route, &error);
            Err(error)
        }
    }
}

async fn stop_to_idle(
    state: &watch::Sender<MediaSessionSnapshot>,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    reason: MediaStopReason,
    cancellation: CancellationToken,
) -> Result<(), MediaSessionActorError> {
    if state.borrow().state == MediaState::Idle {
        set_state(state, MediaState::Idle, None, None, None);
        return Ok(());
    }
    set_state(state, MediaState::Stopping, None, None, None);
    match run_stop(state, driver, policy, reason, cancellation).await {
        Ok(()) => {
            set_state(state, MediaState::Idle, None, None, None);
            Ok(())
        }
        Err(error) => {
            fail(state, None, &error);
            Err(error)
        }
    }
}

async fn run_stop(
    state: &watch::Sender<MediaSessionSnapshot>,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    reason: MediaStopReason,
    cancellation: CancellationToken,
) -> Result<(), MediaSessionActorError> {
    let generation = state.borrow().media_generation;
    if generation == 0 {
        return Ok(());
    }
    run_phase(
        policy,
        "stop media",
        cancellation.clone(),
        driver.stop(generation, reason, cancellation),
    )
    .await
}

async fn rollback_after_start_failure(
    state: &watch::Sender<MediaSessionSnapshot>,
    driver: &mut dyn MediaSessionDriver,
    policy: MediaSessionPolicy,
    request: MediaStartRequest,
    start_error: &MediaSessionActorError,
    cancellation: &ActorCancellation,
) {
    let cancellation = cancellation.cleanup_phase();
    let cleanup = run_phase(
        policy,
        "roll back media",
        cancellation.clone(),
        driver.stop(
            request.media_generation,
            MediaStopReason::TransportFailure,
            cancellation,
        ),
    )
    .await;
    let diagnostic = match cleanup {
        Ok(()) => start_error.to_string(),
        Err(cleanup) => bounded_text(
            format!("{start_error}; rollback also failed: {cleanup}"),
            MAX_ERROR_BYTES,
        ),
    };
    set_state(
        state,
        MediaState::Failed,
        Some(request.route),
        None,
        Some(diagnostic),
    );
}

async fn run_phase<F>(
    policy: MediaSessionPolicy,
    phase: &'static str,
    cancellation: CancellationToken,
    future: F,
) -> Result<(), MediaSessionActorError>
where
    F: std::future::Future<Output = Result<(), MediaDriverError>>,
{
    match timeout(policy.phase_timeout, future).await {
        Ok(result) => result.map_err(|source| MediaSessionActorError::Driver { phase, source }),
        Err(_) => {
            cancellation.cancel();
            Err(MediaSessionActorError::PhaseTimeout {
                phase,
                timeout: policy.phase_timeout,
            })
        }
    }
}

fn cleanup_phase() -> CancellationToken {
    // The explicit shutdown command owns this phase and its absolute deadline;
    // no subsequent user request is allowed to supersede it.
    CancellationToken::new()
}

fn fail(
    state: &watch::Sender<MediaSessionSnapshot>,
    route: Option<MediaRoute>,
    error: &MediaSessionActorError,
) {
    set_state(
        state,
        MediaState::Failed,
        route,
        None,
        Some(bounded_text(error.to_string(), MAX_ERROR_BYTES)),
    );
}

fn set_state(
    state: &watch::Sender<MediaSessionSnapshot>,
    media: MediaState,
    route: Option<MediaRoute>,
    generation: Option<u64>,
    last_error: Option<String>,
) {
    state.send_modify(|snapshot| {
        let generation = generation.unwrap_or(snapshot.media_generation);
        if snapshot.state == media
            && snapshot.route == route
            && snapshot.media_generation == generation
            && snapshot.last_error == last_error
        {
            return;
        }
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.state = media;
        snapshot.route = route;
        snapshot.media_generation = generation;
        snapshot.last_error = last_error;
    });
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
        use crate::display_state::{ActiveRoute, AttachmentState, DisplayTopology};

        let mut state = DisplayRuntimeState::attached(1);
        assert_eq!(MediaRoute::from_display_state(&state), None);
        state.observe_topology(DisplayTopology {
            attachment: AttachmentState::Attached,
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
        assert_eq!(handle.snapshot().state, MediaState::Running);
        assert_eq!(handle.snapshot().media_generation, 1);
        handle.deactivate().await.unwrap();
        assert_eq!(handle.snapshot().state, MediaState::Idle);
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

    #[tokio::test]
    async fn mode_change_stops_before_starting_a_new_generation() {
        let driver = FakeDriver::default();
        let actor = MediaSessionActor::spawn(Box::new(driver.clone())).unwrap();
        let handle = actor.handle();
        handle.activate(route(1, 1920)).await.unwrap();
        handle.activate(route(2, 1280)).await.unwrap();

        assert_eq!(handle.snapshot().state, MediaState::Running);
        assert_eq!(handle.snapshot().media_generation, 2);
        assert_eq!(handle.snapshot().route, Some(route(2, 1280)));
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
        assert_eq!(handle.snapshot().state, MediaState::Failed);
        assert!(handle
            .snapshot()
            .last_error
            .as_deref()
            .unwrap()
            .contains("encoder refused"));

        handle.retry().await.unwrap();
        assert_eq!(handle.snapshot().state, MediaState::Running);
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
        while handle.snapshot().state != MediaState::StartingCapture {
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
        assert_eq!(handle.snapshot().state, MediaState::Idle);

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
        assert_eq!(snapshot.state, MediaState::Suspended);
        assert_eq!(snapshot.media_generation, 1);
        assert_eq!(snapshot.route, Some(route(1, 1920)));
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
        let requests = RequestCoordinator {
            installed_phase: Mutex::new(CancellationToken::new()),
            latest_generation: AtomicU64::new(2),
        };
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
        while handle.snapshot().state != MediaState::Stopping {
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
