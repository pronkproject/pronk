//! Kernel-display adapter that includes capture-pipeline health.

use async_trait::async_trait;

use crate::kernel_display_port::{
    KernelDisplayError, KernelDisplayEvent, KernelDisplayMetadata, KernelDisplayObservation,
    KernelDisplayPort,
};
use crate::media_pipeline_port::{CaptureEvent, CaptureEventPort};

/// One kernel display whose event stream includes capture-pipeline failures.
#[derive(Debug)]
pub struct KernelDisplayWithCapture<D, E> {
    display: D,
    capture_events: Option<E>,
}

impl<D, E> KernelDisplayWithCapture<D, E> {
    pub fn new(display: D, capture_events: E) -> Self {
        Self {
            display,
            capture_events: Some(capture_events),
        }
    }
}

#[async_trait]
impl<D, E> KernelDisplayPort for KernelDisplayWithCapture<D, E>
where
    D: KernelDisplayPort,
    E: CaptureEventPort,
{
    fn metadata(&self) -> KernelDisplayMetadata {
        self.display.metadata()
    }

    fn initial_observation(&self) -> KernelDisplayObservation {
        self.display.initial_observation()
    }

    async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError> {
        let Some(capture_events) = self.capture_events.as_mut() else {
            return self.display.next_event().await;
        };

        tokio::select! {
            event = capture_events.next_event() => match event {
                Some(CaptureEvent::Failed {
                    media_generation,
                    error,
                }) => Ok(KernelDisplayEvent::MediaFailed {
                    media_generation: Some(media_generation),
                    error,
                }),
                None => {
                    self.capture_events = None;
                    Ok(KernelDisplayEvent::MediaFailed {
                        media_generation: None,
                        error: "capture event stream ended".into(),
                    })
                }
            },
            event = self.display.next_event() => event,
        }
    }

    async fn detach(self: Box<Self>) -> Result<(), KernelDisplayError> {
        let Self { display, .. } = *self;
        Box::new(display).detach().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::num::NonZeroU64;

    use super::*;
    use crate::display_state::{DisplayGrantState, DisplayTopology};
    use tokio::sync::mpsc;

    #[derive(Debug)]
    struct TestDisplay {
        events: mpsc::UnboundedReceiver<KernelDisplayEvent>,
    }

    #[async_trait]
    impl KernelDisplayPort for TestDisplay {
        fn metadata(&self) -> KernelDisplayMetadata {
            KernelDisplayMetadata {
                session_id: NonZeroU64::new(1).unwrap(),
            }
        }

        fn initial_observation(&self) -> KernelDisplayObservation {
            KernelDisplayObservation {
                topology: DisplayTopology::Detached,
                grant_state: DisplayGrantState::Active,
            }
        }

        async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError> {
            self.events
                .recv()
                .await
                .ok_or_else(|| KernelDisplayError::new("read test display", "event stream ended"))
        }

        async fn detach(self: Box<Self>) -> Result<(), KernelDisplayError> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct TestCaptureEvents {
        events: VecDeque<CaptureEvent>,
    }

    #[async_trait]
    impl CaptureEventPort for TestCaptureEvents {
        async fn next_event(&mut self) -> Option<CaptureEvent> {
            self.events.pop_front()
        }
    }

    fn display() -> (TestDisplay, mpsc::UnboundedSender<KernelDisplayEvent>) {
        let (events, receive) = mpsc::unbounded_channel();
        (TestDisplay { events: receive }, events)
    }

    fn capture(events: impl IntoIterator<Item = CaptureEvent>) -> TestCaptureEvents {
        TestCaptureEvents {
            events: events.into_iter().collect(),
        }
    }

    #[tokio::test]
    async fn capture_failure_keeps_its_media_generation() {
        let generation = NonZeroU64::new(7).unwrap();
        let (display, _display_events) = display();
        let mut combined = KernelDisplayWithCapture::new(
            display,
            capture([CaptureEvent::Failed {
                media_generation: generation,
                error: "renderer stopped".into(),
            }]),
        );

        assert_eq!(
            combined.next_event().await.unwrap(),
            KernelDisplayEvent::MediaFailed {
                media_generation: Some(generation),
                error: "renderer stopped".into(),
            }
        );
    }

    #[tokio::test]
    async fn ended_capture_stream_reports_once_then_leaves_display_available() {
        let (display, display_events) = display();
        let mut combined = KernelDisplayWithCapture::new(display, capture([]));

        assert_eq!(
            combined.next_event().await.unwrap(),
            KernelDisplayEvent::MediaFailed {
                media_generation: None,
                error: "capture event stream ended".into(),
            }
        );
        display_events.send(KernelDisplayEvent::Revoked).unwrap();
        assert_eq!(
            combined.next_event().await.unwrap(),
            KernelDisplayEvent::Revoked
        );
    }
}
