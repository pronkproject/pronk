//! Source-read preparation before any native submission can be accepted.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::SourceJob;
use pronk_gpu::vulkan::{PendingPrivateRead, SourceImage};

use crate::{ImportedSource, PrivateBuffer};

/// A fullscreen source paired with independently available private storage.
///
/// ```compile_fail
/// use pronk_renderer_worker::PreparedSource;
/// use std::os::fd::AsFd;
///
/// fn pixels_before_submit<F: AsFd>(source: PreparedSource<'_, '_, F>) {
///     source.wait();
/// }
/// ```
#[must_use = "submit the prepared source or release it without access"]
pub struct PreparedSource<'job, 'renderer, F: AsFd> {
    job: SourceJob<'job, 'renderer, F>,
    image: SourceImage,
    destination: PrivateBuffer,
}

impl<'job, 'renderer, F: AsFd> PreparedSource<'job, 'renderer, F> {
    /// Validate the initial fullscreen profile before native source access.
    pub fn new(
        source: ImportedSource<'job, 'renderer, F>,
        destination: PrivateBuffer,
    ) -> Result<Self, SourcePreparationError<ImportedSource<'job, 'renderer, F>>> {
        if let Err(error) = validate(&source, &destination) {
            return Err(SourcePreparationError {
                source: Box::new(source),
                destination,
                error,
            });
        }
        let ImportedSource { job, image, .. } = source;
        Ok(Self {
            job,
            image,
            destination,
        })
    }

    /// Cancel before submission and return the still-unused private image.
    pub fn release_without_access(
        self,
    ) -> Result<PrivateBuffer, SourceReleaseError<PreparedSource<'job, 'renderer, F>>> {
        let Self {
            job,
            image,
            destination,
        } = self;
        match job.release_without_access() {
            Ok(()) => {
                drop(image);
                Ok(destination)
            }
            Err(error) => {
                let (job, error) = error.into_parts();
                Err(SourceReleaseError {
                    source: Box::new(Self {
                        job,
                        image,
                        destination,
                    }),
                    error,
                })
            }
        }
    }

    /// Submit the source read into the reserved private destination.
    ///
    /// An error retains the source job but does not authorize a no-access
    /// release, because a native failure may follow accepted GPU work. Treat
    /// the renderer endpoint as terminal after that result.
    pub fn submit(
        self,
    ) -> Result<
        SubmittedSource<'job, 'renderer, F>,
        SourceSubmissionError<SourceJob<'job, 'renderer, F>>,
    > {
        let Self {
            job,
            image,
            destination,
        } = self;
        let PrivateBuffer {
            identity,
            image: destination,
        } = destination;
        match image.submit_private_copy(destination) {
            Ok(pending) => Ok(SubmittedSource {
                job,
                identity,
                pending,
            }),
            Err(error) => Err(SourceSubmissionError {
                _job: Box::new(job),
                error,
            }),
        }
    }
}

/// Failed read preparation retaining both still-unused resources.
pub struct SourcePreparationError<S> {
    source: Box<S>,
    destination: PrivateBuffer,
    error: io::Error,
}

impl<S> SourcePreparationError<S> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (S, PrivateBuffer, io::Error) {
        (*self.source, self.destination, self.error)
    }
}

/// A native submission failure with no safe normal source-release result.
pub struct SourceSubmissionError<J> {
    _job: Box<J>,
    error: io::Error,
}

impl<J> SourceSubmissionError<J> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }
}

/// Accepted native work awaiting transfer of its completion to CastKMS.
///
/// ```compile_fail
/// use pronk_renderer_worker::SubmittedSource;
/// use std::os::fd::AsFd;
///
/// fn read_before_release<F: AsFd>(source: SubmittedSource<'_, '_, F>) {
///     source.wait();
/// }
/// ```
#[must_use = "release the submitted source completion to CastKMS"]
pub struct SubmittedSource<'job, 'renderer, F: AsFd> {
    job: SourceJob<'job, 'renderer, F>,
    identity: std::sync::Arc<()>,
    pending: PendingPrivateRead,
}

impl<'job, 'renderer, F: AsFd> SubmittedSource<'job, 'renderer, F> {
    /// Transfer the native completion while retaining the pending GPU owner.
    pub fn release(
        self,
    ) -> Result<ReleasedSource, SourceReleaseError<SubmittedSource<'job, 'renderer, F>>> {
        let Self {
            job,
            identity,
            pending,
        } = self;
        match job.release_submitted(pending.completion().map(AsFd::as_fd)) {
            Ok(()) => Ok(ReleasedSource { identity, pending }),
            Err(error) => {
                let (job, error) = error.into_parts();
                Err(SourceReleaseError {
                    source: Box::new(Self {
                        job,
                        identity,
                        pending,
                    }),
                    error,
                })
            }
        }
    }
}

/// A failed CastKMS release retaining the complete worker state for retry.
pub struct SourceReleaseError<S> {
    pub(crate) source: Box<S>,
    pub(crate) error: io::Error,
}

impl<S> SourceReleaseError<S> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_source(self) -> S {
        *self.source
    }

    pub fn into_parts(self) -> (S, io::Error) {
        (*self.source, self.error)
    }
}

/// Native work accepted by CastKMS, with no remaining compositor-source claim.
/// Dropping or waiting for this owner may block while the GPU retires the read.
#[must_use = "retire native work before using or discarding its private pixels"]
pub struct ReleasedSource {
    identity: std::sync::Arc<()>,
    pending: PendingPrivateRead,
}

impl ReleasedSource {
    /// Wait for valid pixels and recover the independently owned private image.
    pub fn wait(self) -> io::Result<PrivateBuffer> {
        let Self { identity, pending } = self;
        pending
            .wait()
            .map(|image| PrivateBuffer { identity, image })
    }
}

fn validate<F: AsFd>(
    source: &ImportedSource<'_, '_, F>,
    destination: &PrivateBuffer,
) -> io::Result<()> {
    let image = source.job.image().extent();
    let geometry = source.geometry;
    let crop = geometry.source();
    let private = destination.extent();
    if crop.origin() != [0, 0]
        || crop.extent() != image
        || geometry.destination() != image
        || geometry.output() != image
        || private.0.get() != image.width()
        || private.1.get() != image.height()
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "renderer source is outside the fullscreen reference profile",
        ));
    }
    Ok(())
}
