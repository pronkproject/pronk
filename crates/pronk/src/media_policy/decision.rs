//! Pure media policy choices and the retry budget for one observed context.

use std::time::Duration;

use crate::display_state::{AttachmentState, DisplayGrantState, MediaState};
use crate::media_session::{MediaRoute, MediaSessionSnapshot, MediaSuspendReason};

use super::{MediaPolicyInput, MediaRecoveryPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicyDecision {
    Action(PolicyAction),
    GiveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicyAction {
    Activate(MediaRoute),
    Deactivate,
    RetryDeactivate(Duration),
    Suspend(MediaSuspendReason),
    Retry(Duration),
}

pub(super) fn decide(
    input: MediaPolicyInput,
    media: &MediaSessionSnapshot,
    retry_delay: Option<Duration>,
) -> Option<PolicyDecision> {
    let Some(route) = input.route else {
        return decide_deactivate(media.state(), retry_delay);
    };
    if input.attachment != AttachmentState::Attached {
        return decide_deactivate(media.state(), retry_delay);
    }
    if !input.device_available || !input.device_session_ready {
        return (media.state() == MediaState::Running).then_some(PolicyDecision::Action(
            PolicyAction::Suspend(MediaSuspendReason::DeviceUnavailable),
        ));
    }
    if input.grant != DisplayGrantState::Active {
        return (media.state() == MediaState::Running).then_some(PolicyDecision::Action(
            PolicyAction::Suspend(MediaSuspendReason::GrantUnavailable),
        ));
    }
    match media.state() {
        MediaState::Running if media.route() == Some(route) => None,
        MediaState::Failed if media.route() == Some(route) => {
            Some(retry_delay.map_or(PolicyDecision::GiveUp, |delay| {
                PolicyDecision::Action(PolicyAction::Retry(delay))
            }))
        }
        _ => Some(PolicyDecision::Action(PolicyAction::Activate(route))),
    }
}

fn decide_deactivate(state: MediaState, retry_delay: Option<Duration>) -> Option<PolicyDecision> {
    match state {
        MediaState::Idle => None,
        // A failed stop remains worth retrying, but it must use the same
        // bounded backoff as failed activation. Otherwise a permanently
        // closed driver port turns the policy actor into a tight loop.
        MediaState::Failed => Some(retry_delay.map_or(PolicyDecision::GiveUp, |delay| {
            PolicyDecision::Action(PolicyAction::RetryDeactivate(delay))
        })),
        _ => Some(PolicyDecision::Action(PolicyAction::Deactivate)),
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
        if self.context != Some(context) || media.state() == MediaState::Running {
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

/// Owns both decision selection and its retry accounting.
#[derive(Debug)]
pub(super) struct PolicyPlanner {
    retry: RetryTracker,
    recovery: MediaRecoveryPolicy,
}

impl PolicyPlanner {
    pub(super) fn new(recovery: MediaRecoveryPolicy) -> Self {
        Self {
            retry: RetryTracker::default(),
            recovery,
        }
    }

    pub(super) fn plan(
        &mut self,
        input: MediaPolicyInput,
        snapshot: &MediaSessionSnapshot,
    ) -> Option<PolicyDecision> {
        self.retry.observe(input, snapshot);
        let decision = decide(input, snapshot, self.retry.next_delay(self.recovery));
        if matches!(
            decision,
            Some(PolicyDecision::Action(
                PolicyAction::Retry(_) | PolicyAction::RetryDeactivate(_)
            ))
        ) {
            self.retry.record_attempt();
        }
        decision
    }
}
