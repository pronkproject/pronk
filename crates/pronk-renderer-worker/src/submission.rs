//! Source-read preparation before any native submission can be accepted.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::SourceJob;
use pronk_gpu::vulkan::{PrivateImage, SourceImage};

use crate::ImportedSource;

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
    destination: PrivateImage,
}

impl<'job, 'renderer, F: AsFd> PreparedSource<'job, 'renderer, F> {
    /// Validate the initial fullscreen profile before native source access.
    pub fn new(
        source: ImportedSource<'job, 'renderer, F>,
        destination: PrivateImage,
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
    ) -> Result<PrivateImage, SourceReleaseError<PreparedSource<'job, 'renderer, F>>> {
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
}

/// Failed read preparation retaining both still-unused resources.
pub struct SourcePreparationError<S> {
    source: Box<S>,
    destination: PrivateImage,
    error: io::Error,
}

impl<S> SourcePreparationError<S> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (S, PrivateImage, io::Error) {
        (*self.source, self.destination, self.error)
    }
}

/// A failed CastKMS release retaining the complete worker state for retry.
pub struct SourceReleaseError<S> {
    source: Box<S>,
    error: io::Error,
}

impl<S> SourceReleaseError<S> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_source(self) -> S {
        *self.source
    }
}

fn validate<F: AsFd>(
    source: &ImportedSource<'_, '_, F>,
    destination: &PrivateImage,
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
