//! One complete scene from a reserved private slot to composed output.

use std::io;
use std::os::fd::AsFd;

use crate::scene_image::{RenderedFrame, SceneImage};
use crate::scene_reads::{PreparedSceneReads, SubmittedSceneReads};
use crate::{PrivateBuffer, QualifiedSceneJob, SceneBuffers, SceneCompositionError, SceneInputs};

impl<'job, F: AsFd> QualifiedSceneJob<'job, F> {
    /// Import sources only after a complete private slot has been reserved.
    pub(crate) fn prepare(
        self,
        buffers: SceneBuffers,
        target: SceneImage,
    ) -> Result<PreparedSceneJob<'job, F>, Box<PrepareSceneJobError<'job, F>>> {
        let sources = match self.import_sources() {
            Ok(sources) => sources,
            Err(cause) => {
                return Err(Box::new(PrepareSceneJobError {
                    scene: self,
                    buffers,
                    target,
                    cause,
                }));
            }
        };
        let SceneBuffers {
            destination,
            sources: destinations,
        } = buffers;
        match PreparedSceneReads::new(self.composer(), sources, destinations) {
            Ok(reads) => Ok(PreparedSceneJob {
                scene: self,
                destination,
                reads,
                target,
            }),
            Err(error) => {
                let (sources, destinations, cause) = error.into_parts();
                drop(sources);
                Err(Box::new(PrepareSceneJobError {
                    scene: self,
                    buffers: SceneBuffers {
                        destination,
                        sources: destinations,
                    },
                    target,
                    cause,
                }))
            }
        }
    }
}

/// Preparation failure retaining the job and the untouched private slot.
pub(crate) struct PrepareSceneJobError<'job, F: AsFd> {
    scene: QualifiedSceneJob<'job, F>,
    buffers: SceneBuffers,
    target: SceneImage,
    cause: io::Error,
}

impl<'job, F: AsFd> PrepareSceneJobError<'job, F> {
    pub(crate) fn into_parts(
        self,
    ) -> (
        QualifiedSceneJob<'job, F>,
        SceneBuffers,
        SceneImage,
        io::Error,
    ) {
        (self.scene, self.buffers, self.target, self.cause)
    }
}

impl<F: AsFd> std::fmt::Debug for PrepareSceneJobError<'_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("PrepareSceneJobError", &self.cause, formatter)
    }
}

impl<F: AsFd> std::fmt::Display for PrepareSceneJobError<'_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "prepare complete scene job: {}", self.cause)
    }
}

impl<F: AsFd> std::error::Error for PrepareSceneJobError<'_, F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// A kernel scene paired with an unused private slot and imported sources.
#[must_use = "submit the scene reads or release the job without source access"]
pub(crate) struct PreparedSceneJob<'job, F: AsFd> {
    scene: QualifiedSceneJob<'job, F>,
    destination: PrivateBuffer,
    reads: PreparedSceneReads,
    target: SceneImage,
}

impl<'job, F: AsFd> PreparedSceneJob<'job, F> {
    /// Submit all source reads while retaining the reserved final image.
    pub(crate) fn submit(
        self,
    ) -> Result<SubmittedSceneJob<'job, F>, Box<SubmitSceneJobError<'job, F>>> {
        match self.reads.submit() {
            Ok(reads) => Ok(SubmittedSceneJob {
                scene: self.scene,
                destination: self.destination,
                reads,
                target: self.target,
            }),
            Err(error) => Err(Box::new(SubmitSceneJobError {
                _scene: Box::new(self.scene),
                destination: self.destination,
                target: self.target,
                cause: error.into_error(),
            })),
        }
    }
}

/// Terminal native submission failure with the still-unused final image.
pub(crate) struct SubmitSceneJobError<'job, F: AsFd> {
    _scene: Box<QualifiedSceneJob<'job, F>>,
    destination: PrivateBuffer,
    target: SceneImage,
    cause: io::Error,
}

impl<F: AsFd> SubmitSceneJobError<'_, F> {
    pub(crate) fn into_parts(self) -> (PrivateBuffer, SceneImage, io::Error) {
        (self.destination, self.target, self.cause)
    }
}

impl<F: AsFd> std::fmt::Debug for SubmitSceneJobError<'_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("SubmitSceneJobError", &self.cause, formatter)
    }
}

impl<F: AsFd> std::fmt::Display for SubmitSceneJobError<'_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "submit complete scene job: {}", self.cause)
    }
}

impl<F: AsFd> std::error::Error for SubmitSceneJobError<'_, F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Native scene reads awaiting composition into their bound registered image.
#[must_use = "finish the scene and release its final completion to CastKMS"]
pub(crate) struct SubmittedSceneJob<'job, F: AsFd> {
    scene: QualifiedSceneJob<'job, F>,
    destination: PrivateBuffer,
    reads: SubmittedSceneReads,
    target: SceneImage,
}

impl<F: AsFd> SubmittedSceneJob<'_, F> {
    /// Finish every source read, compose into registered private storage and
    /// transfer the final native completion before returning any allocation.
    pub(crate) fn render_and_release(self) -> Result<RenderedScene, SceneCompletionError> {
        let (job, composer) = self.scene.into_parts();
        let content_serial = job.content_serial();
        let frames = self
            .reads
            .wait(content_serial)
            .map_err(SceneCompletionError::Source)?;
        let composed = composer
            .compose_and_wait(SceneInputs::new(self.destination, frames, [0; 3]))
            .map_err(SceneCompletionError::Composition)?;
        let (sources, frame) = composed.into_parts();
        let completed = self
            .target
            .write(frame)
            .map_err(SceneCompletionError::PrivateImage)?;
        job.release_submitted(completed.completion.as_fd())
            .map_err(|error| {
                SceneCompletionError::Release(io::Error::new(
                    error.error().kind(),
                    error.error().to_string(),
                ))
            })?;
        Ok(RenderedScene {
            sources,
            frame: completed.frame,
        })
    }
}

/// A published private image and the reusable float sources that produced it.
#[must_use = "return source intermediates and deliver or recycle the rendered frame"]
pub(crate) struct RenderedScene {
    sources: Vec<PrivateBuffer>,
    frame: RenderedFrame,
}

impl RenderedScene {
    pub(crate) fn into_parts(self) -> (Vec<PrivateBuffer>, RenderedFrame) {
        (self.sources, self.frame)
    }
}

/// Failed source completion, composition, private write or kernel release.
#[derive(Debug, thiserror::Error)]
pub enum SceneCompletionError {
    #[error("complete private scene sources: {0}")]
    Source(#[source] io::Error),
    #[error("compose complete private scene: {0}")]
    Composition(#[source] SceneCompositionError),
    #[error("write the registered private scene image: {0}")]
    PrivateImage(#[source] io::Error),
    #[error("release the complete scene job: {0}")]
    Release(#[source] io::Error),
}

fn debug_cause(
    name: &'static str,
    cause: &io::Error,
    formatter: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    formatter.debug_struct(name).field("cause", cause).finish()
}
