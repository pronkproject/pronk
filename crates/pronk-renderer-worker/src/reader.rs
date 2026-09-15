//! Source admission after reserving independent private storage.

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;

use castkms_renderer::{ActiveRenderer, SourceJob};
use pronk_gpu::vulkan::Device;

use crate::{
    ImportedSource, PreparedSource, PrivateBuffer, PrivatePool, RejectedBuffer, ReleasedSource,
    SourceReleaseError,
};

/// Active source endpoint and its bounded private destination pool.
pub struct SourceReader<'renderer, F: AsFd> {
    renderer: ActiveRenderer<'renderer, F>,
    device: Device,
    private: PrivatePool,
}

impl<'renderer, F: AsFd> SourceReader<'renderer, F> {
    /// Allocate every source destination before admitting renderer work.
    pub fn new(
        renderer: ActiveRenderer<'renderer, F>,
        device: Device,
        capacity: NonZeroUsize,
    ) -> Result<Self, SourceReaderStartError<'renderer, F>> {
        let configuration = renderer.configuration();
        let private = match PrivatePool::new(
            &device,
            configuration.width(),
            configuration.height(),
            capacity,
        ) {
            Ok(private) => private,
            Err(error) => {
                return Err(SourceReaderStartError {
                    renderer,
                    device,
                    error,
                });
            }
        };
        Ok(Self {
            renderer,
            device,
            private,
        })
    }

    pub fn available_destinations(&self) -> usize {
        self.private.available()
    }

    /// Reserve storage, claim one changed source, and prepare its native import.
    pub fn try_prepare<'job>(&'job mut self) -> io::Result<SourceOpportunity<'job, 'renderer, F>> {
        let Some(destination) = self.private.take() else {
            return Ok(SourceOpportunity::NoDestination);
        };
        let job = match self.renderer.try_dequeue_source() {
            Ok(Some(job)) => job,
            Ok(None) => {
                restore(&mut self.private, destination)?;
                return Ok(SourceOpportunity::NoSource);
            }
            Err(error) => {
                restore(&mut self.private, destination)?;
                return Err(error);
            }
        };
        let source = match ImportedSource::new(&self.device, job) {
            Ok(source) => source,
            Err(error) => {
                let (job, cause) = error.into_parts();
                return Ok(SourceOpportunity::Rejected(RejectedSource {
                    job,
                    destination,
                    cause,
                }));
            }
        };
        match PreparedSource::new(source, destination) {
            Ok(source) => Ok(SourceOpportunity::Prepared(source)),
            Err(error) => {
                let (source, destination, cause) = error.into_parts();
                let crate::ImportedSource { job, image, .. } = source;
                drop(image);
                Ok(SourceOpportunity::Rejected(RejectedSource {
                    job,
                    destination,
                    cause,
                }))
            }
        }
    }

    /// Attempt one source submission and resolve every claimed kernel job.
    ///
    /// Treat an error as terminal for the active renderer incarnation because
    /// the failing operation may not reveal whether kernel ownership changed.
    pub fn try_submit(&mut self) -> Result<SourceAttempt, SourceAttemptError> {
        match self.try_prepare().map_err(SourceAttemptError::Prepare)? {
            SourceOpportunity::NoDestination => Ok(SourceAttempt::NoDestination),
            SourceOpportunity::NoSource => Ok(SourceAttempt::NoSource),
            SourceOpportunity::Rejected(source) => match source.release() {
                Ok((destination, cause)) => {
                    self.return_destination(destination)
                        .map_err(|_| SourceAttemptError::ReturnDestination)?;
                    Ok(SourceAttempt::Rejected { cause })
                }
                Err(failure) => {
                    let (source, error) = failure.into_parts();
                    drop(source);
                    Err(SourceAttemptError::ReleaseUnused(error))
                }
            },
            SourceOpportunity::Prepared(source) => {
                let submitted = source
                    .submit()
                    .map_err(|failure| SourceAttemptError::Submit(failure.into_error()))?;
                submitted
                    .release()
                    .map(SourceAttempt::Submitted)
                    .map_err(|failure| {
                        let (source, error) = failure.into_parts();
                        drop(source);
                        SourceAttemptError::ReleaseSubmitted(error)
                    })
            }
        }
    }

    pub fn return_destination(&mut self, buffer: PrivateBuffer) -> Result<(), RejectedBuffer> {
        self.private.put(buffer)
    }

    pub fn into_parts(self) -> (ActiveRenderer<'renderer, F>, Device, PrivatePool) {
        (self.renderer, self.device, self.private)
    }
}

/// Result of one source-submission attempt.
#[must_use = "handle idle, rejected, or submitted source work"]
pub enum SourceAttempt {
    NoDestination,
    NoSource,
    Rejected { cause: io::Error },
    Submitted(ReleasedSource),
}

/// Terminal failure while resolving a source-submission attempt.
#[derive(Debug, thiserror::Error)]
pub enum SourceAttemptError {
    #[error("prepare CastKMS source work: {0}")]
    Prepare(#[source] io::Error),
    #[error("release an unused CastKMS source: {0}")]
    ReleaseUnused(#[source] io::Error),
    #[error("return an unused private destination to its pool")]
    ReturnDestination,
    #[error("submit a native CastKMS source read: {0}")]
    Submit(#[source] io::Error),
    #[error("release a submitted CastKMS source read: {0}")]
    ReleaseSubmitted(#[source] io::Error),
}

/// Failed source-reader setup retaining active renderer and device ownership.
pub struct SourceReaderStartError<'renderer, F: AsFd> {
    renderer: ActiveRenderer<'renderer, F>,
    device: Device,
    error: io::Error,
}

impl<'renderer, F: AsFd> SourceReaderStartError<'renderer, F> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (ActiveRenderer<'renderer, F>, Device, io::Error) {
        (self.renderer, self.device, self.error)
    }
}

/// Result of one nonblocking source admission attempt.
#[must_use = "process or release every claimed source"]
pub enum SourceOpportunity<'job, 'renderer, F: AsFd> {
    NoDestination,
    NoSource,
    Rejected(RejectedSource<'job, 'renderer, F>),
    Prepared(PreparedSource<'job, 'renderer, F>),
}

/// A source rejected before pixel access and awaiting a no-access release.
#[must_use = "release the rejected source without access"]
pub struct RejectedSource<'job, 'renderer, F: AsFd> {
    job: SourceJob<'job, 'renderer, F>,
    destination: PrivateBuffer,
    cause: io::Error,
}

impl<'job, 'renderer, F: AsFd> RejectedSource<'job, 'renderer, F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    /// Release the unused source and recover its private destination.
    pub fn release(
        self,
    ) -> Result<(PrivateBuffer, io::Error), SourceReleaseError<RejectedSource<'job, 'renderer, F>>>
    {
        let Self {
            job,
            destination,
            cause,
        } = self;
        match job.release_without_access() {
            Ok(()) => Ok((destination, cause)),
            Err(error) => {
                let (job, error) = error.into_parts();
                Err(SourceReleaseError {
                    source: Box::new(Self {
                        job,
                        destination,
                        cause,
                    }),
                    error,
                })
            }
        }
    }
}

fn restore(pool: &mut PrivatePool, destination: PrivateBuffer) -> io::Result<()> {
    pool.put(destination)
        .map_err(|_| io::Error::other("private destination pool rejected its own buffer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    #[test]
    fn completed_source_attempt_can_move_to_a_blocking_worker() {
        assert_send::<SourceAttempt>();
    }
}
