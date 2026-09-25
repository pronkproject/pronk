//! Async execution of policy choices against the media session actor.

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::media_session::{MediaSessionActorError, MediaSessionHandle};

use super::decision::{PolicyAction, PolicyDecision, PolicyPlanner};
use super::{MediaPolicyEvent, MediaPolicyInput, MediaRecoveryPolicy};

pub(super) async fn run_policy(
    mut input: watch::Receiver<MediaPolicyInput>,
    media: MediaSessionHandle,
    recovery: MediaRecoveryPolicy,
    events: mpsc::UnboundedSender<MediaPolicyEvent>,
    cancellation: CancellationToken,
) {
    let mut media_state = media.subscribe();
    let mut planner = PolicyPlanner::new(recovery);
    loop {
        let observed = *input.borrow_and_update();
        let snapshot = media.snapshot();
        let Some(decision) = planner.plan(observed, &snapshot) else {
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

        let action = match decision {
            PolicyDecision::Action(action) => action,
            PolicyDecision::GiveUp => {
                let error = snapshot
                    .last_error()
                    .unwrap_or("media recovery budget exhausted")
                    .to_owned();
                let _ = events.send(MediaPolicyEvent::RecoveryExhausted { error });
                return;
            }
        };

        let decision_cancellation = CancellationToken::new();
        let operation = apply_action(&media, action, decision_cancellation.child_token());
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

async fn apply_action(
    media: &MediaSessionHandle,
    action: PolicyAction,
    cancellation: CancellationToken,
) -> Result<(), MediaSessionActorError> {
    match action {
        PolicyAction::Activate(route) => media.activate(route).await,
        PolicyAction::Deactivate => media.deactivate().await,
        PolicyAction::RetryDeactivate(delay) => {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                _ = tokio::time::sleep(delay) => media.deactivate().await,
            }
        }
        PolicyAction::Suspend(reason) => media.suspend(reason).await,
        PolicyAction::Retry(delay) => {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                _ = tokio::time::sleep(delay) => media.retry().await,
            }
        }
    }
}
