//! One complete scene from a reserved private slot to composed output.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{SceneJob, SourceReleaseError};

use crate::scene_job::{ReadyScene, ReleasedSceneReads};
use crate::scene_reads::{PreparedSceneReads, SubmittedSceneReads};
use crate::{
    ComposedFrame, PrivateBuffer, QualifiedSceneJob, SceneBuffers, SceneComposer,
    SceneCompositionError, SceneFrames,
};

impl<'job, 'renderer, F: AsFd> QualifiedSceneJob<'job, 'renderer, F> {
    /// Import sources only after a complete private slot has been reserved.
    pub fn prepare(
        self,
        buffers: SceneBuffers,
    ) -> Result<PreparedSceneJob<'job, 'renderer, F>, Box<PrepareSceneJobError<'job, 'renderer, F>>>
    {
        let sources = match self.import_sources() {
            Ok(sources) => sources,
            Err(cause) => {
                return Err(Box::new(PrepareSceneJobError {
                    scene: self,
                    buffers,
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
                    cause,
                }))
            }
        }
    }
}

/// Preparation failure retaining the job and the untouched private slot.
pub struct PrepareSceneJobError<'job, 'renderer, F: AsFd> {
    scene: QualifiedSceneJob<'job, 'renderer, F>,
    buffers: SceneBuffers,
    cause: io::Error,
}

impl<'job, 'renderer, F: AsFd> PrepareSceneJobError<'job, 'renderer, F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(
        self,
    ) -> (
        QualifiedSceneJob<'job, 'renderer, F>,
        SceneBuffers,
        io::Error,
    ) {
        (self.scene, self.buffers, self.cause)
    }
}

impl<F: AsFd> std::fmt::Debug for PrepareSceneJobError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("PrepareSceneJobError", &self.cause, formatter)
    }
}

impl<F: AsFd> std::fmt::Display for PrepareSceneJobError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "prepare complete scene job: {}", self.cause)
    }
}

impl<F: AsFd> std::error::Error for PrepareSceneJobError<'_, '_, F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// A kernel scene paired with an unused private slot and imported sources.
#[must_use = "submit the scene reads or release the job without source access"]
pub struct PreparedSceneJob<'job, 'renderer, F: AsFd> {
    scene: QualifiedSceneJob<'job, 'renderer, F>,
    destination: PrivateBuffer,
    reads: PreparedSceneReads,
}

impl<'job, 'renderer, F: AsFd> PreparedSceneJob<'job, 'renderer, F> {
    /// Cancel before native submission and recover the complete private slot.
    pub fn release_without_access(
        self,
    ) -> Result<SceneBuffers, CancelSceneJobError<SceneJob<'job, 'renderer, F>>> {
        let (sources, destinations) = self.reads.into_parts();
        drop(sources);
        let buffers = SceneBuffers {
            destination: self.destination,
            sources: destinations,
        };
        match self.scene.release_without_access() {
            Ok(()) => Ok(buffers),
            Err(release) => Err(CancelSceneJobError { release, buffers }),
        }
    }

    /// Submit all source reads while retaining the reserved final image.
    pub fn submit(
        self,
    ) -> Result<SubmittedSceneJob<'job, 'renderer, F>, SubmitSceneJobError<'job, 'renderer, F>>
    {
        match self.reads.submit() {
            Ok(reads) => Ok(SubmittedSceneJob {
                scene: self.scene,
                destination: self.destination,
                reads,
            }),
            Err(error) => Err(SubmitSceneJobError {
                _scene: Box::new(self.scene),
                destination: self.destination,
                cause: error.into_error(),
            }),
        }
    }
}

/// Failed no-access release retaining its kernel retry owner and private slot.
pub struct CancelSceneJobError<J> {
    release: SourceReleaseError<J>,
    buffers: SceneBuffers,
}

impl<J> CancelSceneJobError<J> {
    pub fn cause(&self) -> &io::Error {
        self.release.error()
    }

    pub fn into_parts(self) -> (SourceReleaseError<J>, SceneBuffers) {
        (self.release, self.buffers)
    }
}

impl<J> std::fmt::Debug for CancelSceneJobError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("CancelSceneJobError", self.release.error(), formatter)
    }
}

impl<J> std::fmt::Display for CancelSceneJobError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cancel complete scene job: {}",
            self.release.error()
        )
    }
}

impl<J> std::error::Error for CancelSceneJobError<J> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.release.error())
    }
}

/// Terminal native submission failure with the still-unused final image.
pub struct SubmitSceneJobError<'job, 'renderer, F: AsFd> {
    _scene: Box<QualifiedSceneJob<'job, 'renderer, F>>,
    destination: PrivateBuffer,
    cause: io::Error,
}

impl<F: AsFd> SubmitSceneJobError<'_, '_, F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (PrivateBuffer, io::Error) {
        (self.destination, self.cause)
    }
}

impl<F: AsFd> std::fmt::Debug for SubmitSceneJobError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("SubmitSceneJobError", &self.cause, formatter)
    }
}

impl<F: AsFd> std::fmt::Display for SubmitSceneJobError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "submit complete scene job: {}", self.cause)
    }
}

impl<F: AsFd> std::error::Error for SubmitSceneJobError<'_, '_, F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Native scene reads awaiting transfer of their aggregate completion.
///
/// ```compile_fail
/// use pronk_renderer_worker::SubmittedSceneJob;
/// use std::os::fd::AsFd;
///
/// fn compose_before_release<F: AsFd>(scene: SubmittedSceneJob<'_, '_, F>) {
///     scene.compose_and_wait();
/// }
/// ```
#[must_use = "release the aggregate completion to CastKMS"]
pub struct SubmittedSceneJob<'job, 'renderer, F: AsFd> {
    scene: QualifiedSceneJob<'job, 'renderer, F>,
    destination: PrivateBuffer,
    reads: SubmittedSceneReads,
}

impl<'job, 'renderer, F: AsFd> SubmittedSceneJob<'job, 'renderer, F> {
    pub fn release(
        self,
    ) -> Result<ReleasedSceneJob, Box<ReleaseSubmittedSceneError<'job, 'renderer, F>>> {
        match self.scene.release_submitted(self.reads) {
            Ok(reads) => Ok(ReleasedSceneJob {
                destination: self.destination,
                reads,
            }),
            Err(error) => {
                let (scene, reads, cause) = error.into_parts();
                Err(Box::new(ReleaseSubmittedSceneError {
                    submitted: Self {
                        scene,
                        destination: self.destination,
                        reads,
                    },
                    cause,
                }))
            }
        }
    }
}

/// Failed aggregate release retaining the complete submitted transaction.
pub struct ReleaseSubmittedSceneError<'job, 'renderer, F: AsFd> {
    submitted: SubmittedSceneJob<'job, 'renderer, F>,
    cause: io::Error,
}

impl<'job, 'renderer, F: AsFd> ReleaseSubmittedSceneError<'job, 'renderer, F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (SubmittedSceneJob<'job, 'renderer, F>, io::Error) {
        (self.submitted, self.cause)
    }
}

impl<F: AsFd> std::fmt::Debug for ReleaseSubmittedSceneError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("ReleaseSubmittedSceneError", &self.cause, formatter)
    }
}

impl<F: AsFd> std::fmt::Display for ReleaseSubmittedSceneError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "release submitted scene job: {}", self.cause)
    }
}

impl<F: AsFd> std::error::Error for ReleaseSubmittedSceneError<'_, '_, F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Released source reads paired with their reserved final image.
///
/// ```compile_fail
/// use pronk_renderer_worker::ReleasedSceneJob;
///
/// fn inspect_sources_before_wait(scene: ReleasedSceneJob) {
///     scene.into_parts();
/// }
/// ```
#[must_use = "wait for valid private sources before composition"]
pub struct ReleasedSceneJob {
    destination: PrivateBuffer,
    reads: ReleasedSceneReads,
}

impl ReleasedSceneJob {
    /// Wait for released source reads and synchronously compose their scene.
    pub fn compose_and_wait(self) -> Result<ComposedFrame, SceneCompletionError> {
        self.wait()
            .map_err(SceneCompletionError::Source)?
            .compose_and_wait()
            .map_err(SceneCompletionError::Composition)
    }

    pub fn wait(self) -> Result<CompositableScene, SceneWaitError> {
        match self.reads.wait() {
            Ok(scene) => Ok(CompositableScene {
                destination: self.destination,
                scene,
            }),
            Err(cause) => Err(SceneWaitError {
                destination: self.destination,
                cause,
            }),
        }
    }
}

/// Failed source completion or composition for an already released scene.
#[derive(Debug, thiserror::Error)]
pub enum SceneCompletionError {
    #[error("complete private scene sources: {0}")]
    Source(#[source] SceneWaitError),
    #[error("compose complete private scene: {0}")]
    Composition(#[source] SceneCompositionError),
}

/// Source completion failure retaining the final image that was never written.
pub struct SceneWaitError {
    destination: PrivateBuffer,
    cause: io::Error,
}

impl SceneWaitError {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (PrivateBuffer, io::Error) {
        (self.destination, self.cause)
    }
}

impl std::fmt::Debug for SceneWaitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_cause("SceneWaitError", &self.cause, formatter)
    }
}

impl std::fmt::Display for SceneWaitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "wait for complete scene sources: {}", self.cause)
    }
}

impl std::error::Error for SceneWaitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Valid private sources and the final image reserved before their admission.
#[must_use = "compose the complete scene or recover its private owners"]
pub struct CompositableScene {
    destination: PrivateBuffer,
    scene: ReadyScene,
}

impl CompositableScene {
    pub fn compose_and_wait(self) -> Result<ComposedFrame, SceneCompositionError> {
        self.scene.compose_and_wait(self.destination)
    }

    pub fn into_parts(self) -> (SceneComposer, SceneFrames, PrivateBuffer) {
        let (composer, frames) = self.scene.into_parts();
        (composer, frames, self.destination)
    }
}

fn debug_cause(
    name: &'static str,
    cause: &io::Error,
    formatter: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    formatter.debug_struct(name).field("cause", cause).finish()
}
