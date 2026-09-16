//! Aggregate native source reads for one complete scene transaction.

use std::io;
use std::num::NonZeroU64;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

#[cfg(test)]
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::SourceImage;

use crate::pool::BufferIdentity;
use crate::{PrivateBuffer, PrivateFrame, SceneComposer, SceneFrames, SourceAlpha, SubmittedReads};

/// One imported scene layer before it is bound to private staging storage.
#[must_use = "prepare the source for its scene or discard it without pixel access"]
pub(crate) struct SceneSource {
    profile: Arc<()>,
    image: SourceImage,
    alpha: SourceAlpha,
}

impl SceneSource {
    /// Import one checked scene layer without consuming its aggregate job.
    pub(crate) fn import(
        profile: &Arc<()>,
        device: &pronk_gpu::vulkan::Device,
        layer: &castkms_renderer::SceneLayer,
        producer: Option<BorrowedFd<'_>>,
    ) -> io::Result<Self> {
        crate::source::import_image(device, layer.image(), producer).map(|(image, alpha)| Self {
            profile: Arc::clone(profile),
            image,
            alpha,
        })
    }

    /// Bind the DRM format's alpha meaning to a compatible native import.
    #[cfg(test)]
    pub(crate) fn from_drm_format(
        composer: &SceneComposer,
        image: SourceImage,
        format: u32,
    ) -> io::Result<Self> {
        let alpha = crate::source::source_alpha(format, image.layout().format)?;
        Ok(Self {
            profile: Arc::clone(composer.profile()),
            image,
            alpha,
        })
    }
}

struct PreparedRead {
    source: SceneSource,
    destination: PrivateBuffer,
}

/// A complete set of checked layer reads that has not accessed source pixels.
#[must_use = "submit the scene reads or recover every unused owner"]
pub(crate) struct PreparedSceneReads {
    reads: Vec<PreparedRead>,
}

impl PreparedSceneReads {
    /// Match every ordered source and private stage before native submission.
    pub fn new(
        composer: &SceneComposer,
        sources: Vec<SceneSource>,
        destinations: Vec<PrivateBuffer>,
    ) -> Result<Self, PrepareSceneReadsError> {
        if sources.is_empty()
            || sources.len() != composer.layer_count()
            || destinations.len() != composer.layer_count()
            || sources.iter().zip(&destinations).enumerate().any(
                |(index, (source, destination))| {
                    !Arc::ptr_eq(&source.profile, composer.profile())
                        || !composer.accepts_source_stage(index, &source.image, destination)
                },
            )
        {
            return Err(PrepareSceneReadsError {
                sources,
                destinations,
                cause: invalid("scene reads do not match the qualified layer profile"),
            });
        }
        let mut reads = Vec::new();
        if let Err(cause) = reads.try_reserve_exact(sources.len()) {
            return Err(PrepareSceneReadsError {
                sources,
                destinations,
                cause: io::Error::other(cause),
            });
        }
        reads.extend(
            sources
                .into_iter()
                .zip(destinations)
                .map(|(source, destination)| PreparedRead {
                    source,
                    destination,
                }),
        );
        Ok(Self { reads })
    }

    /// Submit every whole-image read and prepare one aggregate completion.
    ///
    /// Any error after the first accepted submission is terminal for the scene
    /// transaction. Cleanup retires accepted native work, but no completion is
    /// returned for a normal kernel release.
    pub fn submit(self) -> Result<SubmittedSceneReads, SubmitSceneReadsError> {
        let mut identities = Vec::new();
        let mut pending = Vec::new();
        let count = self.reads.len();
        if let Err(error) = identities
            .try_reserve_exact(count)
            .and_then(|()| pending.try_reserve_exact(count))
        {
            return Err(SubmitSceneReadsError(io::Error::other(error)));
        }
        for read in self.reads {
            let SceneSource {
                profile: _,
                image,
                alpha,
            } = read.source;
            let PrivateBuffer {
                identity,
                image: destination,
            } = read.destination;
            let submitted = image
                .submit_private_copy(destination)
                .map_err(SubmitSceneReadsError)?;
            identities.push((identity, alpha));
            pending.push(submitted);
        }
        let reads = SubmittedReads::new(pending)
            .map_err(|error| SubmitSceneReadsError(error.into_parts().1))?;
        Ok(SubmittedSceneReads { identities, reads })
    }
}

/// Inputs rejected before any source pixel access.
pub(crate) struct PrepareSceneReadsError {
    sources: Vec<SceneSource>,
    destinations: Vec<PrivateBuffer>,
    cause: io::Error,
}

impl std::fmt::Debug for PrepareSceneReadsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrepareSceneReadsError")
            .field("source_count", &self.sources.len())
            .field("destination_count", &self.destinations.len())
            .field("cause", &self.cause)
            .finish()
    }
}

impl std::fmt::Display for PrepareSceneReadsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "prepare complete scene reads: {}", self.cause)
    }
}

impl std::error::Error for PrepareSceneReadsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl PrepareSceneReadsError {
    #[cfg(test)]
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (Vec<SceneSource>, Vec<PrivateBuffer>, io::Error) {
        (self.sources, self.destinations, self.cause)
    }
}

/// Accepted reads represented by one native completion record.
#[must_use = "wait for every accepted source read before composing the scene"]
pub(crate) struct SubmittedSceneReads {
    identities: Vec<(std::sync::Arc<BufferIdentity>, SourceAlpha)>,
    reads: SubmittedReads,
}

impl SubmittedSceneReads {
    #[cfg(test)]
    pub fn completion(&self) -> Option<&SyncFile> {
        self.reads.completion()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.reads.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    /// Wait for every read and attach the kernel scene's content identity.
    pub(crate) fn wait(self, content_serial: NonZeroU64) -> io::Result<SceneFrames> {
        let images = self.reads.wait()?;
        let layers = self
            .identities
            .into_iter()
            .zip(images)
            .map(|((identity, alpha), image)| PrivateFrame {
                buffer: PrivateBuffer { identity, image },
                content_serial: Some(content_serial),
                alpha,
            })
            .collect();
        SceneFrames::new(content_serial, layers).map_err(|error| error.into_parts().1)
    }
}

/// Terminal failure while submitting a complete set of source reads.
#[derive(Debug, thiserror::Error)]
#[error("submit complete scene reads: {0}")]
pub(crate) struct SubmitSceneReadsError(#[source] io::Error);

impl SubmitSceneReadsError {
    pub fn into_error(self) -> io::Error {
        self.0
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use drm_display_executor::scene::{
        blend::Blend,
        color::{ColorPipeline, OutputColor},
        geometry::{DestinationRect, Extent, SourceRect},
        transform::Transform,
    };
    use pronk_gpu::vulkan::{
        Device, LayerRequirements, PackedFormat, SceneRequirements, SourceRequirements,
    };

    use castkms_sys::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

    use super::*;
    use crate::{SceneBuffers, SceneInputs};

    fn device() -> (Device, Device, u64) {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
        let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
        let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
        (
            Device::open(&node).unwrap(),
            Device::open(node).unwrap(),
            modifier,
        )
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn complete_scene_reads_have_one_completion_and_ordered_frames() {
        let (producer, worker, modifier) = device();
        let extent = Extent::new(4, 3).unwrap();
        let source = SourceRequirements {
            format: PackedFormat::Bgra8,
            extent,
            modifier,
        };
        let layers = [
            LayerRequirements {
                source,
                crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
                destination: DestinationRect {
                    position: [0, 0],
                    extent,
                },
                transform: Transform::default(),
                blend: Blend::default(),
                color: ColorPipeline::new(&[]),
            },
            LayerRequirements {
                source,
                crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
                destination: DestinationRect {
                    position: [1, 0],
                    extent,
                },
                transform: Transform::default(),
                blend: Blend::default(),
                color: ColorPipeline::new(&[]),
            },
        ];
        let composer = SceneComposer::new(
            &worker,
            SceneRequirements {
                output: extent,
                layers: &layers,
                color: OutputColor::default(),
            },
        )
        .unwrap();
        let mut pool = composer
            .create_pool(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap())
            .unwrap();
        let SceneBuffers {
            destination,
            sources: mut destinations,
        } = pool.take().unwrap().unwrap();
        let mut originals = Vec::new();
        let mut sources = Vec::new();
        for (rgba, format) in [
            ([17, 85, 204, 255], DRM_FORMAT_XRGB8888),
            ([231, 57, 19, 128], DRM_FORMAT_ARGB8888),
        ] {
            let (image, completion) = producer
                .allocate(
                    extent.width().try_into().unwrap(),
                    extent.height().try_into().unwrap(),
                    modifier,
                )
                .unwrap()
                .clear_rgba_and_wait(rgba)
                .unwrap();
            // SAFETY: Exact allocator metadata, same physical GPU and a
            // completed foreign release. Originals stay unchanged until wait.
            let imported = unsafe {
                worker.import_source(image.export().unwrap(), image.layout(), completion)
            }
            .unwrap();
            originals.push(image);
            sources.push(
                SceneSource::from_drm_format(&composer, imported, format)
                    .expect("matching DRM source format was rejected"),
            );
        }

        let other_composer = SceneComposer::new(
            &worker,
            SceneRequirements {
                output: extent,
                layers: &layers,
                color: OutputColor::default(),
            },
        )
        .unwrap();
        let mut other_pool = other_composer
            .create_pool(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap())
            .unwrap();
        let other_buffers = other_pool.take().unwrap().unwrap();
        let rejected = PreparedSceneReads::new(&other_composer, sources, other_buffers.sources)
            .err()
            .unwrap();
        assert_eq!(rejected.cause().kind(), io::ErrorKind::InvalidInput);
        let (sources, other_sources, _) = rejected.into_parts();
        assert!(other_pool.restore_sources(other_sources).is_ok());
        assert!(other_pool
            .restore_destination(other_buffers.destination)
            .is_ok());

        destinations.swap(0, 1);
        let rejected = PreparedSceneReads::new(&composer, sources, destinations)
            .err()
            .unwrap();
        assert_eq!(rejected.cause().kind(), io::ErrorKind::InvalidInput);
        let (sources, mut destinations, _) = rejected.into_parts();
        destinations.swap(0, 1);
        let submitted = PreparedSceneReads::new(&composer, sources, destinations)
            .unwrap_or_else(|_| panic!("matching scene reads were rejected"))
            .submit()
            .unwrap();
        assert_eq!(submitted.len(), 2);
        assert!(!submitted.is_empty());
        if let Some(completion) = submitted.completion() {
            assert!(completion.completion().is_ok());
        }
        let serial = NonZeroU64::new(73).unwrap();
        let frames = submitted.wait(serial).unwrap();
        assert_eq!(frames.len(), 2);
        for image in originals {
            image.clear_and_wait([255; 3]).unwrap();
        }
        let composed = composer
            .compose_and_wait(SceneInputs::new(destination, frames, [0; 3]))
            .unwrap();
        let (sources, frame) = composed.into_parts();
        assert_eq!(frame.content_serial(), Some(serial));
        assert!(pool.restore_sources(sources).is_ok());
        assert!(pool.restore_destination(frame.buffer).is_ok());
        assert_eq!(pool.available(), 1);
    }
}
