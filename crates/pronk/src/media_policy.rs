//! Per-display policy that turns observed route/authority state into media commands.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::display_state::{AttachmentState, DisplayGrantState, MediaState};
use crate::media_session::{
    MediaRoute, MediaSessionActor, MediaSessionActorError, MediaSessionDriver, MediaSessionHandle,
    MediaSessionSnapshot, MediaStopReason, MediaSuspendReason,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPolicyInput {
    pub attachment: AttachmentState,
    pub grant: DisplayGrantState,
    pub device_available: bool,
    pub device_session_ready: bool,
    pub device_session_generation: u64,
    pub route: Option<MediaRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaRecoveryPolicy {
    pub maximum_attempts: u32,
    pub initial_delay: Duration,
    pub maximum_delay: Duration,
}

impl Default for MediaRecoveryPolicy {
    fn default() -> Self {
        Self {
            maximum_attempts: 6,
            initial_delay: Duration::from_millis(250),
            maximum_delay: Duration::from_secs(4),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyDecision {
    Activate(MediaRoute),
    Deactivate,
    GiveUp,
    RetryDeactivate(Duration),
    Suspend(MediaSuspendReason),
    Retry(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPolicyEvent {
    RecoveryExhausted { error: String },
}

pub struct DisplayMediaPolicyActor {
    input: watch::Sender<MediaPolicyInput>,
    media: Option<MediaSessionActor>,
    events: mpsc::UnboundedReceiver<MediaPolicyEvent>,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for DisplayMediaPolicyActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DisplayMediaPolicyActor")
            .field("input", &self.input.borrow())
            .field("media", &self.media.as_ref().map(MediaSessionActor::handle))
            .finish_non_exhaustive()
    }
}

impl DisplayMediaPolicyActor {
    pub fn spawn(
        driver: Box<dyn MediaSessionDriver>,
        initial: MediaPolicyInput,
    ) -> Result<Self, MediaSessionActorError> {
        Self::spawn_with_recovery_policy(driver, initial, MediaRecoveryPolicy::default())
    }

    fn spawn_with_recovery_policy(
        driver: Box<dyn MediaSessionDriver>,
        initial: MediaPolicyInput,
        recovery: MediaRecoveryPolicy,
    ) -> Result<Self, MediaSessionActorError> {
        let media = MediaSessionActor::spawn(driver)?;
        let handle = media.handle();
        let (input, input_rx) = watch::channel(initial);
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_policy(
            input_rx,
            handle,
            recovery,
            events,
            cancellation.child_token(),
        ));
        Ok(Self {
            input,
            media: Some(media),
            events: event_rx,
            cancellation,
            task: Some(task),
        })
    }

    pub fn observe(&self, input: MediaPolicyInput) {
        self.input.send_if_modified(|current| {
            if *current == input {
                false
            } else {
                *current = input;
                true
            }
        });
    }

    pub fn snapshot(&self) -> MediaSessionSnapshot {
        self.media
            .as_ref()
            .expect("live media policy owns its media actor")
            .handle()
            .snapshot()
    }

    pub fn subscribe(&self) -> watch::Receiver<MediaSessionSnapshot> {
        self.media
            .as_ref()
            .expect("live media policy owns its media actor")
            .handle()
            .subscribe()
    }

    pub async fn next_event(&mut self) -> Option<MediaPolicyEvent> {
        self.events.recv().await
    }

    pub async fn report_failure(&self, error: String) -> Result<(), MediaSessionActorError> {
        self.media
            .as_ref()
            .expect("live media policy owns its media actor")
            .handle()
            .report_failure(error)
            .await
    }

    pub async fn retry(&self) -> Result<(), MediaSessionActorError> {
        self.media
            .as_ref()
            .expect("live media policy owns its media actor")
            .handle()
            .retry()
            .await
    }

    pub async fn shutdown(mut self, reason: MediaStopReason) -> Result<(), MediaSessionActorError> {
        // A policy decision can be awaiting non-supersedable media cleanup.
        // Interrupt that cleanup before joining the policy task so orderly
        // shutdown is bounded by the media actor's single shutdown budget.
        self.media
            .as_ref()
            .expect("live media policy owns its media actor")
            .begin_shutdown();
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| MediaSessionActorError::Join(error.to_string()))?;
        }
        self.media
            .take()
            .expect("live media policy owns its media actor")
            .shutdown(reason)
            .await
    }
}

impl Drop for DisplayMediaPolicyActor {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_policy(
    mut input: watch::Receiver<MediaPolicyInput>,
    media: MediaSessionHandle,
    recovery: MediaRecoveryPolicy,
    events: mpsc::UnboundedSender<MediaPolicyEvent>,
    cancellation: CancellationToken,
) {
    let mut media_state = media.subscribe();
    let mut retry = RetryTracker::default();
    loop {
        let observed = *input.borrow_and_update();
        let snapshot = media.snapshot();
        retry.observe(observed, &snapshot);
        let retry_delay = retry.next_delay(recovery);
        let Some(decision) = decide(observed, &snapshot, retry_delay) else {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                result = input.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                result = media_state.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
            }
            continue;
        };

        if decision == PolicyDecision::GiveUp {
            let error = snapshot
                .last_error
                .unwrap_or_else(|| "media recovery budget exhausted".into());
            let _ = events.send(MediaPolicyEvent::RecoveryExhausted { error });
            return;
        }

        if matches!(
            decision,
            PolicyDecision::Retry(_) | PolicyDecision::RetryDeactivate(_)
        ) {
            retry.record_attempt();
        }

        let decision_cancellation = CancellationToken::new();
        let operation = apply_decision(&media, decision, decision_cancellation.child_token());
        tokio::pin!(operation);
        tokio::select! {
            _ = cancellation.cancelled() => {
                decision_cancellation.cancel();
                media.cancel_phase();
                let _ = operation.await;
                return;
            }
            result = input.changed() => {
                decision_cancellation.cancel();
                if result.is_err() {
                    media.cancel_phase();
                    let _ = operation.await;
                    return;
                }
                media.cancel_phase();
                let _ = operation.await;
            }
            _ = &mut operation => {}
        }
    }
}

async fn apply_decision(
    media: &MediaSessionHandle,
    decision: PolicyDecision,
    cancellation: CancellationToken,
) -> Result<(), MediaSessionActorError> {
    match decision {
        PolicyDecision::Activate(route) => media.activate(route).await,
        PolicyDecision::Deactivate => media.deactivate().await,
        PolicyDecision::GiveUp => {
            unreachable!("terminal policy decisions are handled by owner notification")
        }
        PolicyDecision::RetryDeactivate(delay) => {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                _ = tokio::time::sleep(delay) => media.deactivate().await,
            }
        }
        PolicyDecision::Suspend(reason) => media.suspend(reason).await,
        PolicyDecision::Retry(delay) => {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                _ = tokio::time::sleep(delay) => media.retry().await,
            }
        }
    }
}

fn decide(
    input: MediaPolicyInput,
    media: &MediaSessionSnapshot,
    retry_delay: Option<Duration>,
) -> Option<PolicyDecision> {
    let Some(route) = input.route else {
        return decide_deactivate(media.state, retry_delay);
    };
    if input.attachment != AttachmentState::Attached {
        return decide_deactivate(media.state, retry_delay);
    }
    if !input.device_available || !input.device_session_ready {
        return (media.state == MediaState::Running).then_some(PolicyDecision::Suspend(
            MediaSuspendReason::DeviceUnavailable,
        ));
    }
    if input.grant != DisplayGrantState::Active {
        return (media.state == MediaState::Running).then_some(PolicyDecision::Suspend(
            MediaSuspendReason::GrantUnavailable,
        ));
    }
    match media.state {
        MediaState::Running if media.route == Some(route) => None,
        MediaState::Failed if media.route == Some(route) => {
            Some(retry_delay.map_or(PolicyDecision::GiveUp, PolicyDecision::Retry))
        }
        _ => Some(PolicyDecision::Activate(route)),
    }
}

fn decide_deactivate(state: MediaState, retry_delay: Option<Duration>) -> Option<PolicyDecision> {
    match state {
        MediaState::Idle => None,
        // A failed stop remains worth retrying, but it must use the same
        // bounded backoff as failed activation. Otherwise a permanently
        // closed driver port turns the policy actor into a tight loop.
        MediaState::Failed => {
            Some(retry_delay.map_or(PolicyDecision::GiveUp, PolicyDecision::RetryDeactivate))
        }
        _ => Some(PolicyDecision::Deactivate),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryContext {
    route: Option<MediaRoute>,
    attachment: AttachmentState,
    grant: DisplayGrantState,
    device_available: bool,
    device_session_ready: bool,
    device_session_generation: u64,
}

impl From<MediaPolicyInput> for RetryContext {
    fn from(input: MediaPolicyInput) -> Self {
        Self {
            route: input.route,
            attachment: input.attachment,
            grant: input.grant,
            device_available: input.device_available,
            device_session_ready: input.device_session_ready,
            device_session_generation: input.device_session_generation,
        }
    }
}

#[derive(Debug, Default)]
struct RetryTracker {
    context: Option<RetryContext>,
    attempts: u32,
}

impl RetryTracker {
    fn observe(&mut self, input: MediaPolicyInput, media: &MediaSessionSnapshot) {
        let context = RetryContext::from(input);
        if self.context != Some(context) || media.state == MediaState::Running {
            self.context = Some(context);
            self.attempts = 0;
        }
    }

    fn next_delay(&self, policy: MediaRecoveryPolicy) -> Option<Duration> {
        if self.attempts >= policy.maximum_attempts {
            return None;
        }
        let multiplier = 1_u32.checked_shl(self.attempts.min(30)).unwrap_or(u32::MAX);
        Some(
            policy
                .initial_delay
                .saturating_mul(multiplier)
                .min(policy.maximum_delay),
        )
    }

    fn record_attempt(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::time::{timeout, Duration};

    use super::*;
    use crate::display_state::{RouteTarget, RoutedMode};
    use crate::media_session::{MediaDriverError, MediaStartRequest};

    fn route(generation: u64) -> MediaRoute {
        MediaRoute {
            route_generation: generation,
            target: RouteTarget::new(NonZeroU32::new(7).unwrap()),
            mode: RoutedMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            },
        }
    }

    fn input(route: Option<MediaRoute>) -> MediaPolicyInput {
        MediaPolicyInput {
            attachment: AttachmentState::Attached,
            grant: DisplayGrantState::Active,
            device_available: true,
            device_session_ready: true,
            device_session_generation: 1,
            route,
        }
    }

    fn snapshot(state: MediaState, route: Option<MediaRoute>) -> MediaSessionSnapshot {
        MediaSessionSnapshot {
            revision: 1,
            media_generation: u64::from(route.is_some()),
            state,
            route,
            last_error: None,
        }
    }

    #[test]
    fn only_an_authorized_available_active_route_activates_media() {
        assert_eq!(
            decide(
                input(Some(route(1))),
                &snapshot(MediaState::Idle, None),
                Some(Duration::ZERO)
            ),
            Some(PolicyDecision::Activate(route(1)))
        );
        let mut unavailable = input(Some(route(1)));
        unavailable.device_available = false;
        assert_eq!(
            decide(
                unavailable,
                &snapshot(MediaState::Running, Some(route(1))),
                Some(Duration::ZERO)
            ),
            Some(PolicyDecision::Suspend(
                MediaSuspendReason::DeviceUnavailable
            ))
        );
        let mut recovering = input(Some(route(1)));
        recovering.device_session_ready = false;
        assert_eq!(
            decide(
                recovering,
                &snapshot(MediaState::Running, Some(route(1))),
                Some(Duration::ZERO)
            ),
            Some(PolicyDecision::Suspend(
                MediaSuspendReason::DeviceUnavailable
            ))
        );
        let mut suspended_grant = input(Some(route(1)));
        suspended_grant.grant = DisplayGrantState::SuspendedOtherMaster;
        assert_eq!(
            decide(
                suspended_grant,
                &snapshot(MediaState::Running, Some(route(1))),
                Some(Duration::ZERO)
            ),
            Some(PolicyDecision::Suspend(
                MediaSuspendReason::GrantUnavailable
            ))
        );
    }

    #[test]
    fn a_failed_route_retries_only_while_the_budget_allows() {
        assert_eq!(
            decide(
                input(Some(route(1))),
                &snapshot(MediaState::Failed, Some(route(1))),
                Some(Duration::from_millis(250))
            ),
            Some(PolicyDecision::Retry(Duration::from_millis(250)))
        );
        assert_eq!(
            decide(
                input(Some(route(1))),
                &snapshot(MediaState::Failed, Some(route(1))),
                None
            ),
            Some(PolicyDecision::GiveUp)
        );
        assert_eq!(
            decide(
                input(Some(route(2))),
                &snapshot(MediaState::Failed, Some(route(1))),
                None
            ),
            Some(PolicyDecision::Activate(route(2)))
        );
        assert_eq!(
            decide(
                input(None),
                &snapshot(MediaState::Failed, Some(route(1))),
                None
            ),
            Some(PolicyDecision::GiveUp)
        );
        assert_eq!(
            decide(
                input(None),
                &snapshot(MediaState::Failed, Some(route(1))),
                Some(Duration::from_millis(250))
            ),
            Some(PolicyDecision::RetryDeactivate(Duration::from_millis(250)))
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Capture(u64),
        Media(u64),
        Suspend(u64),
        Stop(u64, MediaStopReason),
        Shutdown(MediaStopReason),
    }

    #[derive(Debug, Clone, Default)]
    struct FakeDriver {
        calls: Arc<Mutex<Vec<Call>>>,
        fail_capture_attempts: Arc<AtomicU32>,
        fail_stop_attempts: Arc<AtomicU32>,
        block_stop_once: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl MediaSessionDriver for FakeDriver {
        async fn start_capture(
            &mut self,
            request: MediaStartRequest,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Capture(request.media_generation));
            if self
                .fail_capture_attempts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(MediaDriverError::new("PipeWire is restarting"));
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
            Ok(())
        }

        async fn suspend(
            &mut self,
            media_generation: u64,
            _reason: MediaSuspendReason,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaDriverError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Suspend(media_generation));
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
            if self
                .fail_stop_attempts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(MediaDriverError::new("capture driver is unavailable"));
            }
            if self.block_stop_once.swap(false, Ordering::SeqCst) {
                cancellation.cancelled().await;
                return Err(MediaDriverError::new("stop cancelled by owner shutdown"));
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

    async fn wait_for_state(
        state: &mut watch::Receiver<MediaSessionSnapshot>,
        expected: MediaState,
    ) {
        timeout(Duration::from_secs(1), async {
            while state.borrow().state != expected {
                state.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn policy_drives_route_grant_and_disable_lifecycle_in_order() {
        let driver = FakeDriver::default();
        let policy = DisplayMediaPolicyActor::spawn(Box::new(driver.clone()), input(None)).unwrap();
        let mut state = policy.subscribe();

        policy.observe(input(Some(route(1))));
        wait_for_state(&mut state, MediaState::Running).await;

        let mut unavailable_grant = input(Some(route(1)));
        unavailable_grant.grant = DisplayGrantState::SuspendedOtherMaster;
        policy.observe(unavailable_grant);
        wait_for_state(&mut state, MediaState::Suspended).await;

        policy.observe(input(Some(route(1))));
        wait_for_state(&mut state, MediaState::Running).await;
        assert_eq!(state.borrow().media_generation, 2);

        policy.observe(input(None));
        wait_for_state(&mut state, MediaState::Idle).await;
        policy
            .shutdown(MediaStopReason::DisplayRemoved)
            .await
            .unwrap();

        assert_eq!(
            *driver.calls.lock().unwrap(),
            vec![
                Call::Capture(1),
                Call::Media(1),
                Call::Suspend(1),
                Call::Stop(1, MediaStopReason::ModeChanged),
                Call::Capture(2),
                Call::Media(2),
                Call::Stop(2, MediaStopReason::OutputDisabled),
                Call::Shutdown(MediaStopReason::DisplayRemoved),
            ]
        );
    }

    #[tokio::test]
    async fn owner_shutdown_interrupts_a_policy_cleanup_already_in_flight() {
        let driver = FakeDriver::default();
        let policy = DisplayMediaPolicyActor::spawn(Box::new(driver.clone()), input(None)).unwrap();
        let mut state = policy.subscribe();

        policy.observe(input(Some(route(1))));
        wait_for_state(&mut state, MediaState::Running).await;
        driver.block_stop_once.store(true, Ordering::SeqCst);
        policy.observe(input(None));
        wait_for_state(&mut state, MediaState::Stopping).await;

        timeout(
            Duration::from_secs(1),
            policy.shutdown(MediaStopReason::BackendShutdown),
        )
        .await
        .expect("policy shutdown remained blocked on an earlier cleanup")
        .unwrap();
        assert_eq!(
            driver.calls.lock().unwrap().last(),
            Some(&Call::Shutdown(MediaStopReason::BackendShutdown))
        );
    }

    #[tokio::test]
    async fn policy_retries_transient_media_failure_with_fresh_generations() {
        let driver = FakeDriver {
            fail_capture_attempts: Arc::new(AtomicU32::new(2)),
            ..FakeDriver::default()
        };
        let policy = DisplayMediaPolicyActor::spawn_with_recovery_policy(
            Box::new(driver.clone()),
            input(None),
            MediaRecoveryPolicy {
                maximum_attempts: 3,
                initial_delay: Duration::from_millis(1),
                maximum_delay: Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut state = policy.subscribe();

        policy.observe(input(Some(route(1))));
        wait_for_state(&mut state, MediaState::Running).await;
        assert_eq!(state.borrow().media_generation, 3);
        policy
            .shutdown(MediaStopReason::DisplayRemoved)
            .await
            .unwrap();

        let calls = driver.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::Capture(_)))
                .count(),
            3
        );
        assert!(calls.contains(&Call::Media(3)));
    }

    #[tokio::test]
    async fn policy_reports_exhausted_activation_recovery_to_its_owner() {
        let driver = FakeDriver {
            fail_capture_attempts: Arc::new(AtomicU32::new(u32::MAX)),
            ..FakeDriver::default()
        };
        let mut policy = DisplayMediaPolicyActor::spawn_with_recovery_policy(
            Box::new(driver.clone()),
            input(None),
            MediaRecoveryPolicy {
                maximum_attempts: 2,
                initial_delay: Duration::from_millis(1),
                maximum_delay: Duration::from_millis(1),
            },
        )
        .unwrap();

        policy.observe(input(Some(route(1))));
        let event = timeout(Duration::from_secs(1), policy.next_event())
            .await
            .expect("media recovery never reached its retry limit")
            .expect("media policy stopped without reporting terminal failure");
        assert!(matches!(event, MediaPolicyEvent::RecoveryExhausted { .. }));
        assert_eq!(policy.snapshot().state, MediaState::Failed);
        assert_eq!(
            driver
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| matches!(call, Call::Capture(_)))
                .count(),
            3
        );
        policy
            .shutdown(MediaStopReason::DisplayRemoved)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn policy_bounds_failed_deactivation_retries() {
        let driver = FakeDriver {
            fail_stop_attempts: Arc::new(AtomicU32::new(u32::MAX)),
            ..FakeDriver::default()
        };
        let mut policy = DisplayMediaPolicyActor::spawn_with_recovery_policy(
            Box::new(driver.clone()),
            input(None),
            MediaRecoveryPolicy {
                maximum_attempts: 2,
                initial_delay: Duration::from_millis(1),
                maximum_delay: Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut state = policy.subscribe();

        policy.observe(input(Some(route(1))));
        wait_for_state(&mut state, MediaState::Running).await;
        policy.observe(input(None));
        wait_for_state(&mut state, MediaState::Failed).await;

        let event = timeout(Duration::from_secs(1), policy.next_event())
            .await
            .expect("media cleanup never reached its retry limit")
            .expect("media policy stopped without reporting terminal failure");
        assert!(matches!(event, MediaPolicyEvent::RecoveryExhausted { .. }));
        assert_eq!(
            driver
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| matches!(call, Call::Stop(..)))
                .count(),
            3
        );
        assert!(policy
            .shutdown(MediaStopReason::DisplayRemoved)
            .await
            .is_err());
    }

    #[test]
    fn fresh_device_session_generation_resets_an_exhausted_retry_budget() {
        let policy = MediaRecoveryPolicy {
            maximum_attempts: 2,
            initial_delay: Duration::from_millis(1),
            maximum_delay: Duration::from_millis(2),
        };
        let failed = snapshot(MediaState::Failed, Some(route(1)));
        let mut retry = RetryTracker::default();
        let first = input(Some(route(1)));
        retry.observe(first, &failed);
        retry.record_attempt();
        retry.record_attempt();
        assert_eq!(retry.next_delay(policy), None);

        let mut replacement = first;
        replacement.device_session_generation = 2;
        retry.observe(replacement, &failed);
        assert_eq!(retry.next_delay(policy), Some(Duration::from_millis(1)));
    }
}
