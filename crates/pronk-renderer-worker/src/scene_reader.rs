//! Complete-scene admission after reserving independent private storage.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{OutputChannel, PublishedRenderer};

use crate::scene_image::RegisteredSceneImages;
use crate::{QualifiedSceneJob, RenderedFrame, ScenePool, SceneStorageProfile};

/// Active scene endpoint and the reusable private pool for its storage profile.
pub struct SceneReader<F: AsFd> {
    renderer: PublishedRenderer<F>,
    pub(crate) output: OutputChannel,
    storage: SceneStorageProfile,
    private: ScenePool,
    images: RegisteredSceneImages,
}

impl<F: AsFd> SceneReader<F> {
    /// Bind the active endpoint to an already allocated matching scene pool.
    pub fn new(
        mut renderer: PublishedRenderer<F>,
        storage: SceneStorageProfile,
        private: ScenePool,
        images: RegisteredSceneImages,
    ) -> Result<Self, Box<SceneReaderStartError<F>>> {
        let output = storage.output();
        if !private.belongs_to(storage.profile()) || output != renderer.output() {
            return Err(Box::new(SceneReaderStartError {
                renderer,
                storage,
                private: Box::new(private),
                cause: invalid("scene pool does not match the active renderer profile"),
            }));
        }
        let output = match renderer.open_output_channel() {
            Ok(output) => output,
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
            output,
            storage,
            private,
            images,
        })
    }

    pub fn available_slots(&self) -> usize {
        self.private.available().min(self.images.available())
    }

    pub(crate) fn device(&self) -> &pronk_gpu::vulkan::Device {
        self.storage.device()
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
            match self.renderer.try_acquire_job(target.registration()) {
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
                    return Err(SceneAttemptError::Acquire(cause));
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
        if scene.composer().layer_count() == 0 {
            let crate::SceneBuffers {
                destination,
                sources,
            } = buffers;
            let frame = scene
                .render_blank(destination, target)
                .map_err(SceneAttemptError::Complete)?;
            self.private
                .restore_sources(sources)
                .map_err(|_| SceneAttemptError::ReturnSlot)?;
            return Ok(SceneAttempt::Rendered(frame));
        }
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

    /// Stop new selection and source admission before closing this generation.
    pub fn withdraw(self) -> io::Result<()> {
        let Self {
            renderer,
            output,
            storage,
            private,
            images,
        } = self;
        drop(output);
        let result = match renderer.withdraw() {
            Ok(renderer) => {
                drop(renderer);
                Ok(())
            }
            Err(error) => {
                let (renderer, error) = error.into_parts();
                drop(renderer);
                Err(error)
            }
        };
        // Closing the endpoint removes its registrations before their paired
        // native images and private storage are destroyed.
        drop(images);
        drop(private);
        drop(storage);
        result
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
    /// No changed scene is available; an active blank change is a renderable job.
    NoScene,
    Rejected {
        cause: io::Error,
    },
    Rendered(RenderedFrame),
}

/// Terminal failure while resolving a complete-scene submission attempt.
#[derive(Debug, thiserror::Error)]
pub enum SceneAttemptError {
    #[error("reserve a complete private scene slot: {0}")]
    Reserve(#[source] io::Error),
    #[error("acquire a complete CastKMS job: {0}")]
    Acquire(#[source] io::Error),
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
pub struct SceneReaderStartError<F: AsFd> {
    renderer: PublishedRenderer<F>,
    storage: SceneStorageProfile,
    private: Box<ScenePool>,
    cause: io::Error,
}

impl<F: AsFd> SceneReaderStartError<F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(
        self,
    ) -> (
        PublishedRenderer<F>,
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

        assert_send::<SceneReader<std::fs::File>>();
        assert_send::<SceneAttempt>();
        assert_send::<crate::DeliveryAttempt>();
        assert_send::<crate::DeliveryError>();
        assert_send::<SceneReaderStartError<std::fs::File>>();
    }
}
