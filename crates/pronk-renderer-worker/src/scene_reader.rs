//! Complete-scene admission after reserving independent private storage.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::ActiveRenderer;

use crate::scene_image::{PreparedSceneImages, SceneImagePool};
use crate::{QualifiedSceneJob, RenderedFrame, ScenePool, SceneStorageProfile};

/// Active scene endpoint and the reusable private pool for its storage profile.
pub struct SceneReader<'renderer, F: AsFd> {
    renderer: ActiveRenderer<'renderer, F>,
    storage: SceneStorageProfile,
    private: ScenePool,
    images: SceneImagePool,
}

impl<'renderer, F: AsFd> SceneReader<'renderer, F> {
    /// Bind the active endpoint to an already allocated matching scene pool.
    pub fn new(
        renderer: ActiveRenderer<'renderer, F>,
        storage: SceneStorageProfile,
        private: ScenePool,
        images: PreparedSceneImages,
    ) -> Result<Self, Box<SceneReaderStartError<'renderer, F>>> {
        let configuration = renderer.configuration();
        let output = storage.output();
        if !private.belongs_to(storage.profile())
            || output.width() != configuration.width().get()
            || output.height() != configuration.height().get()
            || !images.matches(
                storage.device(),
                configuration.width(),
                configuration.height(),
            )
        {
            return Err(Box::new(SceneReaderStartError {
                renderer,
                storage,
                private: Box::new(private),
                cause: invalid("scene pool does not match the active renderer profile"),
            }));
        }
        let mut renderer = renderer;
        let images = match images.register(&mut renderer) {
            Ok(images) => images,
            Err(cause) => {
                return Err(Box::new(SceneReaderStartError {
                    renderer,
                    storage,
                    private: Box::new(private),
                    cause,
                }));
            }
        };
        Ok(Self {
            renderer,
            storage,
            private,
            images,
        })
    }

    pub fn available_slots(&self) -> usize {
        self.private.available().min(self.images.available())
    }

    /// Render at most one changed complete scene into registered private storage.
    ///
    /// Import, producer waits and failed native cleanup may block. Run on a
    /// blocking graphics worker rather than an asynchronous executor thread.
    /// An error is terminal for this active renderer incarnation.
    pub fn try_render(&mut self) -> Result<SceneAttempt, SceneAttemptError> {
        let Some(buffers) = self.private.take().map_err(SceneAttemptError::Reserve)? else {
            return Ok(SceneAttempt::NoSlot);
        };
        let available = self.images.available();
        let mut deferred = Vec::new();
        if deferred.try_reserve_exact(available).is_err() {
            self.restore(buffers)?;
            return Err(SceneAttemptError::Reserve(io::Error::other(
                "reserve private-image selection storage",
            )));
        }
        let (job, target) = loop {
            let Some(target) = self.images.take() else {
                self.restore(buffers)?;
                return Ok(SceneAttempt::NoSlot);
            };
            match self.renderer.try_dequeue_scene(target.registration()) {
                Ok(Some(job)) => {
                    for image in deferred {
                        self.images
                            .restore(image)
                            .map_err(|_| SceneAttemptError::ReturnSlot)?;
                    }
                    break (job, target);
                }
                Ok(None) => {
                    deferred.push(target);
                    for image in deferred {
                        self.images
                            .restore(image)
                            .map_err(|_| SceneAttemptError::ReturnSlot)?;
                    }
                    self.restore(buffers)?;
                    return Ok(SceneAttempt::NoScene);
                }
                Err(cause) if cause.raw_os_error() == Some(nix::libc::EBUSY) => {
                    deferred.push(target);
                    if deferred.len() == available {
                        for image in deferred {
                            self.images
                                .restore(image)
                                .map_err(|_| SceneAttemptError::ReturnSlot)?;
                        }
                        self.restore(buffers)?;
                        return Ok(SceneAttempt::NoSlot);
                    }
                }
                Err(cause) => {
                    deferred.push(target);
                    for image in deferred {
                        self.images
                            .restore(image)
                            .map_err(|_| SceneAttemptError::ReturnSlot)?;
                    }
                    self.restore(buffers)?;
                    return Err(SceneAttemptError::Dequeue(cause));
                }
            }
        };
        let scene = match QualifiedSceneJob::new(&self.storage, job) {
            Ok(scene) => scene,
            Err(error) => {
                let (job, cause) = error.into_parts();
                match job.release_without_access() {
                    Ok(()) => {
                        self.restore(buffers)?;
                        self.images
                            .restore(target)
                            .map_err(|_| SceneAttemptError::ReturnSlot)?;
                    }
                    Err(error) => {
                        let (_, cause) = error.into_parts();
                        return Err(SceneAttemptError::ReleaseUnused(cause));
                    }
                }
                return Ok(SceneAttempt::Rejected { cause });
            }
        };
        let prepared = match scene.prepare(buffers, target) {
            Ok(prepared) => prepared,
            Err(error) => {
                let (scene, buffers, target, cause) = error.into_parts();
                match scene.release_without_access() {
                    Ok(()) => {
                        self.restore(buffers)?;
                        self.images
                            .restore(target)
                            .map_err(|_| SceneAttemptError::ReturnSlot)?;
                    }
                    Err(error) => {
                        let (_, cause) = error.into_parts();
                        return Err(SceneAttemptError::ReleaseUnused(cause));
                    }
                }
                return Ok(SceneAttempt::Rejected { cause });
            }
        };
        let submitted = prepared.submit().map_err(|error| {
            let (destination, target, cause) = error.into_parts();
            drop(destination);
            drop(target);
            SceneAttemptError::Submit(cause)
        })?;
        let rendered = submitted
            .render_and_release()
            .map_err(SceneAttemptError::Complete)?;
        let (sources, frame) = rendered.into_parts();
        self.private
            .restore_sources(sources)
            .map_err(|_| SceneAttemptError::ReturnSlot)?;
        Ok(SceneAttempt::Rendered(frame))
    }

    pub fn return_frame(&mut self, frame: RenderedFrame) -> Result<(), Box<RenderedFrame>> {
        if !self.private.accepts_destination(&frame.private.buffer)
            || !self.images.accepts(&frame.scene)
        {
            return Err(Box::new(frame));
        }
        self.private
            .restore_destination_validated(frame.private.buffer);
        self.images.restore_validated(frame.scene);
        Ok(())
    }

    fn restore(&mut self, buffers: crate::SceneBuffers) -> Result<(), SceneAttemptError> {
        self.private
            .restore(buffers)
            .map_err(|_| SceneAttemptError::ReturnSlot)
    }
}

/// Outcome of one complete-scene rendering attempt.
#[must_use = "handle idle, rejected or completed scene work"]
pub enum SceneAttempt {
    NoSlot,
    NoScene,
    Rejected { cause: io::Error },
    Rendered(RenderedFrame),
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
    #[error("complete and release the rendered scene: {0}")]
    Complete(#[source] crate::SceneCompletionError),
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
