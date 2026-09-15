//! Source-read preparation before any native submission can be accepted.

use std::io;
use std::num::NonZeroU64;
use std::os::fd::AsFd;

use castkms_renderer::{SourceGeometry, SourceJob};
use pronk_gpu::vulkan::{PendingPrivateRead, SourceImage};

use crate::{ImportedSource, PrivateBuffer, PrivateFrame, SourceAlpha};

/// A source region paired with independently available private output storage.
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
    geometry: SourceGeometry,
    alpha: SourceAlpha,
}

impl<'job, 'renderer, F: AsFd> PreparedSource<'job, 'renderer, F> {
    /// Match the complete output to private storage before native source access.
    pub fn new(
        source: ImportedSource<'job, 'renderer, F>,
        destination: PrivateBuffer,
    ) -> Result<Self, SourcePreparationError<ImportedSource<'job, 'renderer, F>>> {
        if let Err(error) = validate_destination(source.geometry, destination.extent()) {
            return Err(SourcePreparationError {
                source: Box::new(source),
                destination,
                error,
            });
        }
        let ImportedSource {
            job,
            image,
            geometry,
            alpha,
        } = source;
        Ok(Self {
            job,
            image,
            destination,
            geometry,
            alpha,
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
            geometry,
            alpha,
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
                        geometry,
                        alpha,
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
            geometry,
            alpha,
        } = self;
        let PrivateBuffer {
            identity,
            image: destination,
        } = destination;
        match image.submit_private_region(
            destination,
            geometry.source(),
            [0, 0],
            geometry.destination(),
            [0; 3],
        ) {
            Ok(pending) => Ok(SubmittedSource {
                job,
                identity,
                alpha,
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

    /// Discard terminal source ownership and return the submission error.
    pub fn into_error(self) -> io::Error {
        self.error
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
    identity: std::sync::Arc<crate::pool::BufferIdentity>,
    alpha: SourceAlpha,
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
            alpha,
            pending,
        } = self;
        let content_serial = job.content_serial();
        match job.release_submitted(pending.completion().map(AsFd::as_fd)) {
            Ok(()) => Ok(ReleasedSource {
                identity,
                alpha,
                content_serial,
                pending,
            }),
            Err(error) => {
                let (job, error) = error.into_parts();
                Err(SourceReleaseError {
                    source: Box::new(Self {
                        job,
                        identity,
                        alpha,
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
    identity: std::sync::Arc<crate::pool::BufferIdentity>,
    alpha: SourceAlpha,
    content_serial: NonZeroU64,
    pending: PendingPrivateRead,
}

impl ReleasedSource {
    /// Wait for valid pixels and recover the independently owned private image.
    pub fn wait(self) -> io::Result<PrivateFrame> {
        let Self {
            identity,
            alpha,
            content_serial,
            pending,
        } = self;
        pending.wait().map(|image| PrivateFrame {
            buffer: PrivateBuffer { identity, image },
            content_serial: Some(content_serial),
            alpha,
        })
    }
}

fn validate_destination(
    geometry: SourceGeometry,
    private: (std::num::NonZeroU32, std::num::NonZeroU32),
) -> io::Result<()> {
    if private.0.get() != geometry.output().width() || private.1.get() != geometry.output().height()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private destination dimensions do not match the complete output",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use drm_display_executor::scene::geometry::{Extent, SourceRect};

    use super::*;

    fn extent(width: u32, height: u32) -> Extent {
        Extent::new(width, height).unwrap()
    }

    fn private(width: u32, height: u32) -> (NonZeroU32, NonZeroU32) {
        (
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
        )
    }

    #[test]
    fn output_sized_storage_accepts_crops_scaling_and_padding() {
        let image = extent(1920, 1080);
        let output = extent(1280, 720);
        for (origin, crop, destination) in [
            ([0, 0], image, output),
            ([20, 30], extent(640, 360), output),
            ([20, 30], extent(640, 360), extent(320, 180)),
            ([1919, 1079], extent(1, 1), output),
        ] {
            let geometry = SourceGeometry::new(
                SourceRect::new(image, origin, crop).unwrap(),
                destination,
                output,
            )
            .unwrap();
            assert!(validate_destination(geometry, private(1280, 720)).is_ok());
        }
    }

    #[test]
    fn private_storage_must_match_output_not_source_or_plane() {
        let image = extent(1920, 1080);
        let geometry = SourceGeometry::new(
            SourceRect::new(image, [20, 30], extent(640, 360)).unwrap(),
            extent(320, 180),
            extent(1280, 720),
        )
        .unwrap();
        for dimensions in [
            private(1920, 1080),
            private(640, 360),
            private(320, 180),
            private(1281, 720),
            private(1280, 721),
        ] {
            assert_eq!(
                validate_destination(geometry, dimensions)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
