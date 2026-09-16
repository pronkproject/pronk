//! One renderer output generation connected to a private PipeWire source.

use std::io;
use std::num::NonZeroU64;

use pronk_pipewire::{
    PipeWireRemote, VideoNodeIdentity, VideoSourceActor, VideoSourceActorError,
    VideoSourceActorEvent, VideoSourceActorRuntimeError, VideoSourceConfig, VideoSourceGeneration,
};
use pronk_renderer_worker::{
    CompletedOutput, CompletedReturn, FinishedOutput, OutputDestination, OutputReturn,
    PendingOutput, ReadyOutput, RenderedFrame,
};

use crate::{invalid, OutputEvent, OutputSession, PublishError, Registration};

/// One registered pool and its active private PipeWire source.
///
/// Use [`Self::shutdown`] before releasing the authorization domain. Dropping
/// the owner is a terminal best-effort path and does not report retirement.
pub struct Video {
    source: VideoSourceActor,
    output: OutputSession,
}

impl Video {
    /// Start PipeWire with the exact pool consumed by the registration.
    pub async fn start(
        registration: Registration,
        config: VideoSourceConfig,
        remote: PipeWireRemote,
    ) -> io::Result<Self> {
        let expected_generation = config.media_generation;
        let buffers = registration.export()?;
        let source = VideoSourceActor::spawn().map_err(error)?;
        let identity = match source
            .start(VideoSourceGeneration {
                config,
                buffers,
                remote,
            })
            .await
        {
            Ok(identity) => identity,
            Err(cause) => {
                let _ = source.shutdown().await;
                return Err(error(cause));
            }
        };
        if identity.media_generation != expected_generation {
            let _ = source.shutdown().await;
            return Err(invalid("PipeWire returned another media generation"));
        }
        let output = registration.bind(identity);
        Ok(Self { source, output })
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        self.output.identity()
    }

    pub fn claim(&mut self, slot: usize) -> io::Result<OutputDestination> {
        self.output.claim(slot)
    }

    pub fn submit(&mut self, output: CompletedOutput) -> io::Result<PendingOutput> {
        self.output.submit(output)
    }

    pub fn finish(&mut self, output: FinishedOutput) -> io::Result<ReadyOutput> {
        self.output.finish(output)
    }

    pub fn finish_return(&mut self, returned: CompletedReturn) -> io::Result<usize> {
        self.output.finish_return(returned)
    }

    /// Retain publication before crossing the asynchronous source boundary.
    pub async fn publish(
        &mut self,
        output: ReadyOutput,
        pts_ns: i64,
        discontinuity: bool,
    ) -> Result<(RenderedFrame, Option<NonZeroU64>), FramePublishError> {
        let (rendered, frame) = self
            .output
            .publish(output, pts_ns, discontinuity)
            .map_err(FramePublishError::Prepare)?;
        let content_serial = frame.content_serial();
        let media_generation = self.output.identity().media_generation;
        if let Err(cause) = self
            .source
            .publish(media_generation, frame.into_frame())
            .await
        {
            return Err(FramePublishError::Handoff {
                frame: rendered,
                content_serial,
                cause: error(cause),
            });
        }
        Ok((rendered, content_serial))
    }

    /// Receive one source event and translate it into native output ownership.
    pub async fn next_event(&mut self) -> io::Result<VideoEvent> {
        let event = self
            .source
            .next_event()
            .await
            .ok_or_else(|| io::Error::other("PipeWire source event stream closed"))?;
        let failure = match &event {
            VideoSourceActorEvent::GenerationFailed { error, .. } => Some(error.clone()),
            _ => None,
        };
        let output = self.output.handle_event(&event)?;
        match (failure, output) {
            (None, OutputEvent::Ignored) => Ok(VideoEvent::Ignored),
            (None, OutputEvent::Available { slot }) => Ok(VideoEvent::Available { slot }),
            (None, OutputEvent::Released(output)) => Ok(VideoEvent::Released(output)),
            (Some(cause), OutputEvent::Reclaimed(returns)) => {
                Ok(VideoEvent::Failed { cause, returns })
            }
            _ => Err(io::Error::other(
                "PipeWire event produced an inconsistent renderer transition",
            )),
        }
    }

    /// Join PipeWire and return every locally retained publication.
    pub async fn shutdown(self) -> StoppedVideo {
        let Self {
            source, mut output, ..
        } = self;
        let (report, mut cause) = match source.shutdown().await {
            Ok(report) => (report, None),
            Err(VideoSourceActorError::Shutdown { report, source }) => {
                (Some(*report), Some(error(source)))
            }
            Err(cause) => (None, Some(error(cause))),
        };
        let returns = match report {
            Some(report) => match output.stopped(&report) {
                Ok(OutputEvent::Reclaimed(outputs)) => outputs,
                Ok(_) => {
                    cause.get_or_insert_with(|| {
                        io::Error::other(
                            "PipeWire stop produced an inconsistent renderer transition",
                        )
                    });
                    Box::new([])
                }
                Err(error) => {
                    cause.get_or_insert(error);
                    Box::new([])
                }
            },
            None => Box::new([]),
        };
        StoppedVideo {
            output,
            returns,
            cause,
        }
    }
}

/// Renderer action produced by one PipeWire source event.
#[must_use = "apply output availability, retirement, or failure"]
pub enum VideoEvent {
    Ignored,
    Available {
        slot: usize,
    },
    Released(OutputReturn),
    Failed {
        cause: VideoSourceActorRuntimeError,
        returns: Box<[OutputReturn]>,
    },
}

/// Failed frame publication with ownership needed for recovery.
pub enum FramePublishError {
    Prepare(Box<PublishError>),
    Handoff {
        frame: RenderedFrame,
        content_serial: Option<NonZeroU64>,
        cause: io::Error,
    },
}

impl FramePublishError {
    pub fn error(&self) -> &io::Error {
        match self {
            Self::Prepare(error) => error.error(),
            Self::Handoff { cause, .. } => cause,
        }
    }
}

/// Quiesced PipeWire generation and any native reader waits it exposed.
#[must_use = "finish or retain every output reclaimed by shutdown"]
pub struct StoppedVideo {
    output: OutputSession,
    returns: Box<[OutputReturn]>,
    cause: Option<io::Error>,
}

impl StoppedVideo {
    pub fn error(&self) -> Option<&io::Error> {
        self.cause.as_ref()
    }

    pub fn into_parts(self) -> (OutputSession, Box<[OutputReturn]>, Option<io::Error>) {
        (self.output, self.returns, self.cause)
    }

    /// Finish every output reclaimed by shutdown without stopping after an error.
    pub async fn finish(mut self) -> io::Result<()> {
        let mut cause = self.cause.take();
        for returned in self.returns.into_vec() {
            let completed = returned.wait().await;
            if let Err(error) = self.output.finish_return(completed) {
                cause.get_or_insert(error);
            }
        }
        cause.map_or(Ok(()), Err)
    }
}

fn error(cause: impl std::fmt::Display) -> io::Error {
    io::Error::other(cause.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    #[test]
    fn generation_ownership_can_cross_a_supervisor_task_boundary() {
        assert_send::<Registration>();
        assert_send::<Video>();
        assert_send::<VideoEvent>();
        assert_send::<FramePublishError>();
        assert_send::<StoppedVideo>();
    }
}
