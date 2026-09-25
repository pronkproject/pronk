//! Ordered media-session transitions and their resource owner.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, Instant};
use tokio_util::sync::CancellationToken;

use super::{
    bounded_text, ActorCancellation, Command, CommandKind, MediaDriverError, MediaPhase,
    MediaRoute, MediaSessionActorError, MediaSessionDriver, MediaSessionPolicy,
    MediaSessionSnapshot, MediaStartRequest, MediaStopReason, MediaSuspendReason, MAX_ERROR_BYTES,
    MAX_MEDIA_SHUTDOWN_TIMEOUT,
};
use crate::display_state::MediaState;

pub(super) struct ActorRuntime {
    state: watch::Sender<MediaSessionSnapshot>,
    cancellation: ActorCancellation,
    driver: Box<dyn MediaSessionDriver>,
    policy: MediaSessionPolicy,
}

impl ActorRuntime {
    pub(super) fn new(
        state: watch::Sender<MediaSessionSnapshot>,
        cancellation: ActorCancellation,
        driver: Box<dyn MediaSessionDriver>,
        policy: MediaSessionPolicy,
    ) -> Self {
        Self {
            state,
            cancellation,
            driver,
            policy,
        }
    }

    pub(super) async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            let command = tokio::select! {
                _ = self.cancellation.owner_dropped.cancelled() => break,
                command = commands.recv() => command,
            };
            let Some(command) = command else {
                break;
            };
            let Command {
                request_generation,
                kind,
            } = command;
            if !kind.is_shutdown() && !self.cancellation.requests.is_current(request_generation) {
                kind.reject_superseded();
                continue;
            }
            match kind {
                CommandKind::Activate { route, response } => {
                    let result = self.activate(request_generation, route).await;
                    let _ = response.send(result);
                }
                CommandKind::Deactivate { response } => {
                    let result = self
                        .stop_to_idle(
                            self.policy,
                            MediaStopReason::OutputDisabled,
                            self.cancellation.cleanup_phase(),
                        )
                        .await;
                    let _ = response.send(result);
                }
                CommandKind::Suspend { reason, response } => {
                    let result = self.suspend(request_generation, reason).await;
                    let _ = response.send(result);
                }
                CommandKind::Retry { response } => {
                    let snapshot = self.state.borrow().clone();
                    let result = match snapshot.phase {
                        MediaPhase::Failed {
                            route: Some(route), ..
                        } => self.activate(request_generation, route).await,
                        _ => Err(MediaSessionActorError::RetryUnavailable(snapshot.state())),
                    };
                    let _ = response.send(result);
                }
                CommandKind::ReportFailure { error, response } => {
                    let result = self.report_external_failure(error).await;
                    let _ = response.send(result);
                }
                CommandKind::Shutdown { reason, response } => {
                    let result = self.shutdown_driver(reason).await;
                    let _ = response.send(result);
                    return;
                }
            }
        }

        let _ = self.shutdown_driver(MediaStopReason::BackendShutdown).await;
    }

    async fn report_external_failure(
        &mut self,
        error: String,
    ) -> Result<(), MediaSessionActorError> {
        let snapshot = self.state.borrow().clone();
        let cleanup = self
            .run_stop(
                self.policy,
                MediaStopReason::TransportFailure,
                self.cancellation.cleanup_phase(),
            )
            .await;
        let diagnostic = match &cleanup {
            Ok(()) => error,
            Err(cleanup) => bounded_text(
                format!("{error}; cleanup also failed: {cleanup}"),
                MAX_ERROR_BYTES,
            ),
        };
        self.set_phase(
            MediaPhase::Failed {
                route: snapshot.route(),
                error: diagnostic,
            },
            None,
        );
        cleanup
    }

    async fn shutdown_driver(
        &mut self,
        reason: MediaStopReason,
    ) -> Result<(), MediaSessionActorError> {
        // Cleanup and final owner shutdown share one phase budget. Reserving one
        // third for the final call prevents a slow StopMedia from consuming the
        // entire deadline and skipping the resource owner's shutdown hook.
        let shutdown_timeout = self.policy.phase_timeout.min(MAX_MEDIA_SHUTDOWN_TIMEOUT);
        let deadline = Instant::now() + shutdown_timeout;
        let final_reserve = (shutdown_timeout / 3).max(Duration::from_nanos(1));
        let stop_policy = MediaSessionPolicy {
            phase_timeout: shutdown_timeout.saturating_sub(final_reserve),
        };
        let media_result = self
            .stop_to_idle(stop_policy, reason, cleanup_phase())
            .await;
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
            self.driver.shutdown(reason, cancellation),
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
        &mut self,
        request_generation: u64,
        route: MediaRoute,
    ) -> Result<(), MediaSessionActorError> {
        if route.route_generation == 0 {
            return Err(MediaSessionActorError::InvalidRouteGeneration);
        }
        let current = self.state.borrow().clone();
        if current.phase == MediaPhase::Running(route) {
            return Ok(());
        }
        if current.state() == MediaState::Failed {
            // A failed phase may have left protocol authority ambiguous even when
            // its first rollback also failed.  Retry the generation-matched
            // idempotent cleanup before minting a fresh media generation.
            self.set_phase(MediaPhase::Reconfiguring(route), None);
            if let Err(error) = self
                .run_stop(
                    self.policy,
                    MediaStopReason::TransportFailure,
                    self.cancellation.cleanup_phase(),
                )
                .await
            {
                self.fail(Some(route), &error);
                return Err(error);
            }
        } else if current.state() != MediaState::Idle {
            self.set_phase(MediaPhase::Reconfiguring(route), None);
            if let Err(error) = self
                .run_stop(
                    self.policy,
                    MediaStopReason::ModeChanged,
                    self.cancellation.cleanup_phase(),
                )
                .await
            {
                self.fail(Some(route), &error);
                return Err(error);
            }
        }

        let generation = self
            .state
            .borrow()
            .media_generation
            .checked_add(1)
            .ok_or(MediaSessionActorError::GenerationExhausted)?;
        let request = MediaStartRequest {
            media_generation: generation,
            route,
        };
        self.set_phase(MediaPhase::StartingCapture(route), Some(generation));
        let phase_cancellation = self.cancellation.requests.install_phase(request_generation);
        if let Err(error) = run_phase(
            self.policy,
            "start capture",
            phase_cancellation.clone(),
            self.driver.start_capture(request, phase_cancellation),
        )
        .await
        {
            self.rollback_after_start_failure(request, &error).await;
            return Err(error);
        }

        self.set_phase(MediaPhase::StartingMedia(route), None);
        let phase_cancellation = self.cancellation.requests.install_phase(request_generation);
        if let Err(error) = run_phase(
            self.policy,
            "start backend media",
            phase_cancellation.clone(),
            self.driver.start_media(request, phase_cancellation),
        )
        .await
        {
            self.rollback_after_start_failure(request, &error).await;
            return Err(error);
        }

        self.set_phase(MediaPhase::Running(route), None);
        Ok(())
    }

    async fn suspend(
        &mut self,
        request_generation: u64,
        reason: MediaSuspendReason,
    ) -> Result<(), MediaSessionActorError> {
        let snapshot = self.state.borrow().clone();
        let route = match snapshot.phase {
            MediaPhase::Suspended(_) => return Ok(()),
            MediaPhase::Running(route) => route,
            _ => return Err(MediaSessionActorError::SuspendUnavailable(snapshot.state())),
        };
        let phase_cancellation = self.cancellation.requests.install_phase(request_generation);
        let result = run_phase(
            self.policy,
            "suspend media",
            phase_cancellation.clone(),
            self.driver
                .suspend(snapshot.media_generation, reason, phase_cancellation),
        )
        .await;
        match result {
            Ok(()) => {
                self.set_phase(MediaPhase::Suspended(route), None);
                Ok(())
            }
            Err(error) => {
                self.fail(Some(route), &error);
                Err(error)
            }
        }
    }

    async fn stop_to_idle(
        &mut self,
        policy: MediaSessionPolicy,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaSessionActorError> {
        if self.state.borrow().state() == MediaState::Idle {
            self.set_phase(MediaPhase::Idle, None);
            return Ok(());
        }
        self.set_phase(MediaPhase::Stopping, None);
        match self.run_stop(policy, reason, cancellation).await {
            Ok(()) => {
                self.set_phase(MediaPhase::Idle, None);
                Ok(())
            }
            Err(error) => {
                self.fail(None, &error);
                Err(error)
            }
        }
    }

    async fn run_stop(
        &mut self,
        policy: MediaSessionPolicy,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaSessionActorError> {
        let generation = self.state.borrow().media_generation;
        if generation == 0 {
            return Ok(());
        }
        run_phase(
            policy,
            "stop media",
            cancellation.clone(),
            self.driver.stop(generation, reason, cancellation),
        )
        .await
    }

    async fn rollback_after_start_failure(
        &mut self,
        request: MediaStartRequest,
        start_error: &MediaSessionActorError,
    ) {
        let cancellation = self.cancellation.cleanup_phase();
        let cleanup = run_phase(
            self.policy,
            "roll back media",
            cancellation.clone(),
            self.driver.stop(
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
        self.set_phase(
            MediaPhase::Failed {
                route: Some(request.route),
                error: diagnostic,
            },
            None,
        );
    }
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
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => {
            let error = MediaSessionActorError::Driver { phase, source };
            tracing::warn!(phase, %error, "media phase failed");
            Err(error)
        }
        Err(_) => {
            cancellation.cancel();
            let error = MediaSessionActorError::PhaseTimeout {
                phase,
                timeout: policy.phase_timeout,
            };
            tracing::warn!(phase, %error, "media phase timed out");
            Err(error)
        }
    }
}

fn cleanup_phase() -> CancellationToken {
    // The explicit shutdown command owns this phase and its absolute deadline;
    // no subsequent user request is allowed to supersede it.
    CancellationToken::new()
}

impl ActorRuntime {
    fn fail(&self, route: Option<MediaRoute>, error: &MediaSessionActorError) {
        self.set_phase(
            MediaPhase::Failed {
                route,
                error: bounded_text(error.to_string(), MAX_ERROR_BYTES),
            },
            None,
        );
    }

    fn set_phase(&self, phase: MediaPhase, generation: Option<u64>) {
        self.state.send_if_modified(|snapshot| {
            let generation = generation.unwrap_or(snapshot.media_generation);
            if snapshot.phase == phase && snapshot.media_generation == generation {
                return false;
            }
            snapshot.phase = phase;
            snapshot.media_generation = generation;
            true
        });
    }
}
