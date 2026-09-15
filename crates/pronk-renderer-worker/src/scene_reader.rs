//! Complete-scene admission after reserving independent private storage.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::ActiveRenderer;

use crate::{
    ComposedFrame, PrivateBuffer, PrivateFrame, QualifiedSceneJob, RejectedBuffer,
    RejectedComposedFrame, RejectedSceneSources, ReleasedSceneJob, ScenePool, SceneStorageProfile,
};

/// Active scene endpoint and the reusable private pool for its storage profile.
pub struct SceneReader<'renderer, F: AsFd> {
    renderer: ActiveRenderer<'renderer, F>,
    storage: SceneStorageProfile,
    private: ScenePool,
}

impl<'renderer, F: AsFd> SceneReader<'renderer, F> {
    /// Bind the active endpoint to an already allocated matching scene pool.
    pub fn new(
        renderer: ActiveRenderer<'renderer, F>,
        storage: SceneStorageProfile,
        private: ScenePool,
    ) -> Result<Self, Box<SceneReaderStartError<'renderer, F>>> {
        let configuration = renderer.configuration();
        let output = storage.output();
        if !private.belongs_to(storage.profile())
            || output.width() != configuration.width().get()
            || output.height() != configuration.height().get()
        {
            return Err(Box::new(SceneReaderStartError {
                renderer,
                storage,
                private: Box::new(private),
                cause: invalid("scene pool does not match the active renderer profile"),
            }));
        }
        Ok(Self {
            renderer,
            storage,
            private,
        })
    }

    pub fn available_slots(&self) -> usize {
        self.private.available()
    }

    /// Reserve a complete slot and submit at most one changed complete scene.
    ///
    /// Import, producer waits and failed native cleanup may block. Run on a
    /// blocking graphics worker rather than an asynchronous executor thread.
    /// An error is terminal for this active renderer incarnation.
    pub fn try_submit(&mut self) -> Result<SceneAttempt, SceneAttemptError> {
        let Some(buffers) = self.private.take().map_err(SceneAttemptError::Reserve)? else {
            return Ok(SceneAttempt::NoSlot);
        };
        let job = match self.renderer.try_dequeue_scene() {
            Ok(Some(job)) => job,
            Ok(None) => {
                self.restore(buffers)?;
                return Ok(SceneAttempt::NoScene);
            }
            Err(cause) => {
                self.restore(buffers)?;
                return Err(SceneAttemptError::Dequeue(cause));
            }
        };
        let scene = match QualifiedSceneJob::new(&self.storage, job) {
            Ok(scene) => scene,
            Err(error) => {
                let (job, cause) = error.into_parts();
                match job.release_without_access() {
                    Ok(()) => self.restore(buffers)?,
                    Err(error) => {
                        let (_, cause) = error.into_parts();
                        return Err(SceneAttemptError::ReleaseUnused(cause));
                    }
                }
                return Ok(SceneAttempt::Rejected { cause });
            }
        };
        let prepared = match scene.prepare(buffers) {
            Ok(prepared) => prepared,
            Err(error) => {
                let (scene, buffers, cause) = error.into_parts();
                match scene.release_without_access() {
                    Ok(()) => self.restore(buffers)?,
                    Err(error) => {
                        let (_, cause) = error.into_parts();
                        return Err(SceneAttemptError::ReleaseUnused(cause));
                    }
                }
                return Ok(SceneAttempt::Rejected { cause });
            }
        };
        let submitted = prepared.submit().map_err(|error| {
            let (destination, cause) = error.into_parts();
            drop(destination);
            SceneAttemptError::Submit(cause)
        })?;
        submitted
            .release()
            .map(|released| SceneAttempt::Submitted(Box::new(released)))
            .map_err(|error| {
                let (_, cause) = error.into_parts();
                SceneAttemptError::ReleaseSubmitted(cause)
            })
    }

    pub fn return_sources(
        &mut self,
        sources: Vec<PrivateBuffer>,
    ) -> Result<(), RejectedSceneSources> {
        self.private.restore_sources(sources)
    }

    pub fn finish_composition(
        &mut self,
        composed: ComposedFrame,
    ) -> Result<PrivateFrame, RejectedComposedFrame> {
        self.private.finish_composition(composed)
    }

    pub fn return_destination(&mut self, destination: PrivateBuffer) -> Result<(), RejectedBuffer> {
        self.private.restore_destination(destination)
    }

    pub fn into_parts(self) -> (ActiveRenderer<'renderer, F>, SceneStorageProfile, ScenePool) {
        (self.renderer, self.storage, self.private)
    }

    fn restore(&mut self, buffers: crate::SceneBuffers) -> Result<(), SceneAttemptError> {
        self.private
            .restore(buffers)
            .map_err(|_| SceneAttemptError::ReturnSlot)
    }
}

/// Outcome of one nonblocking complete-scene submission attempt.
#[must_use = "handle idle, rejected or submitted complete-scene work"]
pub enum SceneAttempt {
    NoSlot,
    NoScene,
    Rejected { cause: io::Error },
    Submitted(Box<ReleasedSceneJob>),
}

/// Terminal failure while resolving a complete-scene submission attempt.
#[derive(Debug, thiserror::Error)]
pub enum SceneAttemptError {
    #[error("reserve a complete private scene slot: {0}")]
    Reserve(#[source] io::Error),
    #[error("dequeue a complete CastKMS scene: {0}")]
    Dequeue(#[source] io::Error),
    #[error("release an unused complete CastKMS scene: {0}")]
    ReleaseUnused(#[source] io::Error),
    #[error("return an unused complete private scene slot")]
    ReturnSlot,
    #[error("submit complete native scene reads: {0}")]
    Submit(#[source] io::Error),
    #[error("release submitted complete scene reads: {0}")]
    ReleaseSubmitted(#[source] io::Error),
}

/// Failed reader setup retaining the active endpoint and private storage.
pub struct SceneReaderStartError<'renderer, F: AsFd> {
    renderer: ActiveRenderer<'renderer, F>,
    storage: SceneStorageProfile,
    private: Box<ScenePool>,
    cause: io::Error,
}

impl<'renderer, F: AsFd> SceneReaderStartError<'renderer, F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(
        self,
    ) -> (
        ActiveRenderer<'renderer, F>,
        SceneStorageProfile,
        ScenePool,
        io::Error,
    ) {
        (self.renderer, self.storage, *self.private, self.cause)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_scene_work_can_move_to_a_blocking_thread() {
        fn assert_send<T: Send>() {}

        assert_send::<SceneReader<'static, std::fs::File>>();
        assert_send::<SceneAttempt>();
        assert_send::<SceneReaderStartError<'static, std::fs::File>>();
    }
}
