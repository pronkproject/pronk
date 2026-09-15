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
        loop {
            let Some(capture_events) = self.capture_events.as_mut() else {
                return self.display.next_event().await;
            };

            tokio::select! {
                event = self.display.next_event() => return event,
                event = capture_events.next_event() => match event {
                    Some(CaptureEvent::Failed {
                        media_generation,
                        error,
                    }) => {
                        return Ok(KernelDisplayEvent::MediaFailed {
                            media_generation: Some(media_generation),
                            error,
                        });
                    }
                    None => self.capture_events = None,
                },
            }
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
    use crate::display_state::{AttachmentState, DisplayGrantState, DisplayTopology};

    #[derive(Debug)]
    struct TestDisplay {
        events: VecDeque<KernelDisplayEvent>,
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
                topology: DisplayTopology {
                    attachment: AttachmentState::Detached,
                    route: None,
                },
                grant_state: DisplayGrantState::Active,
            }
        }

        async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError> {
            match self.events.pop_front() {
                Some(event) => Ok(event),
                None => std::future::pending().await,
            }
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

    fn display(events: impl IntoIterator<Item = KernelDisplayEvent>) -> TestDisplay {
        TestDisplay {
            events: events.into_iter().collect(),
        }
    }

    fn capture(events: impl IntoIterator<Item = CaptureEvent>) -> TestCaptureEvents {
        TestCaptureEvents {
            events: events.into_iter().collect(),
        }
    }

    #[tokio::test]
    async fn capture_failure_keeps_its_media_generation() {
        let generation = NonZeroU64::new(7).unwrap();
        let mut combined = KernelDisplayWithCapture::new(
            display([]),
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
    async fn ended_capture_stream_leaves_display_events_available() {
        let mut combined =
            KernelDisplayWithCapture::new(display([KernelDisplayEvent::Revoked]), capture([]));

        assert_eq!(
            combined.next_event().await.unwrap(),
            KernelDisplayEvent::Revoked
        );
    }
}
