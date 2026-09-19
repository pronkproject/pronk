//! Generation-specific health for a capture pipeline and its display observer.

use std::num::NonZeroU64;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::media_pipeline_port::{CaptureEvent, CaptureEventPort, MediaPipelineError};

/// Sole consumer of asynchronous capture failures across media generations.
#[derive(Debug)]
pub struct CaptureEvents {
    events: mpsc::UnboundedReceiver<CaptureEvent>,
}

impl CaptureEvents {
    pub(crate) fn channel() -> (mpsc::UnboundedSender<CaptureEvent>, Self) {
        let (send, events) = mpsc::unbounded_channel();
        (send, Self { events })
    }
}

#[async_trait]
impl CaptureEventPort for CaptureEvents {
    async fn next_event(&mut self) -> Option<CaptureEvent> {
        self.events.recv().await
    }
}

/// Reports at most one failure from one capture owner's state stream.
///
/// Cancel before requesting orderly shutdown of the capture owner. Dropping
/// the monitor cancels observation, not capture, native reads or output writes.
pub(crate) struct CaptureMonitor {
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl CaptureMonitor {
    pub(crate) fn watch<S: Send + Sync + 'static>(
        media_generation: NonZeroU64,
        mut state: watch::Receiver<S>,
        events: mpsc::UnboundedSender<CaptureEvent>,
        failure: fn(&S) -> Option<String>,
        closed: &'static str,
    ) -> Self {
        let stop = CancellationToken::new();
        let cancellation = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                if cancellation.is_cancelled() {
                    return;
                }
                let error = failure(&state.borrow_and_update());
                if let Some(error) = error {
                    tracing::warn!(%media_generation, %error, "capture pipeline failed");
                    let _ = events.send(CaptureEvent::Failed {
                        media_generation,
                        error,
                    });
                    return;
                }
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    changed = state.changed() => {
                        if changed.is_err() {
                            tracing::warn!(%media_generation, error = closed,
                                           "capture pipeline health stream ended");
                            let _ = events.send(CaptureEvent::Failed {
                                media_generation,
                                error: closed.into(),
                            });
                            return;
                        }
                    }
                }
            }
        });
        Self {
            stop,
            task: Some(task),
        }
    }

    pub(crate) fn cancel(&self) {
        self.stop.cancel();
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), MediaPipelineError> {
        self.cancel();
        self.task
            .take()
            .expect("live capture monitor owns its task")
            .await
            .map_err(|error| MediaPipelineError::new(format!("join capture monitor: {error}")))
    }
}

impl Drop for CaptureMonitor {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn monitor(state: watch::Receiver<Option<String>>) -> (CaptureMonitor, CaptureEvents) {
        let (send, receive) = CaptureEvents::channel();
        (
            CaptureMonitor::watch(
                NonZeroU64::new(17).unwrap(),
                state,
                send,
                Clone::clone,
                "owner disappeared",
            ),
            receive,
        )
    }

    async fn event(events: &mut CaptureEvents) -> Option<CaptureEvent> {
        tokio::time::timeout(Duration::from_secs(1), events.next_event())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn reports_an_existing_failure_once_with_its_generation() {
        let (state, receive) = watch::channel(Some("output failed".into()));
        let (monitor, mut events) = monitor(receive);
        assert_eq!(
            event(&mut events).await,
            Some(CaptureEvent::Failed {
                media_generation: NonZeroU64::new(17).unwrap(),
                error: "output failed".into(),
            })
        );
        state.send_replace(Some("a second failure".into()));
        assert_eq!(event(&mut events).await, None);
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loss_of_the_state_owner_is_not_a_healthy_stream() {
        let (state, receive) = watch::channel(None);
        let (monitor, mut events) = monitor(receive);
        drop(state);
        assert_eq!(
            event(&mut events).await,
            Some(CaptureEvent::Failed {
                media_generation: NonZeroU64::new(17).unwrap(),
                error: "owner disappeared".into(),
            })
        );
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_the_monitor_stops_observation() {
        let (state, receive) = watch::channel(None);
        let (monitor, mut events) = monitor(receive);
        drop(monitor);
        state.send_replace(Some("retired".into()));
        assert_eq!(event(&mut events).await, None);
    }
}
