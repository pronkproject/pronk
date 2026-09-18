//! Private GPU work used to qualify one renderer configuration.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::RendererConfiguration;
use pronk_gpu::vulkan::{Device, PrivateImage};

/// A renderer configuration paired with completed work over private GPU storage.
///
/// The image is neither a CastKMS scene source nor a capture destination. It
/// exists only to establish that the selected Vulkan device can allocate and
/// execute work for the configured output extent before publication.
///
/// ```compile_fail
/// use pronk_renderer_worker::PrivatePreparation;
/// use std::os::fd::AsFd;
///
/// fn publish_before_completion<F: AsFd>(preparation: PrivatePreparation<F>) {
///     preparation.publish();
/// }
/// ```
#[must_use = "complete private preparation or close the renderer configuration"]
pub struct PrivatePreparation<F: AsFd> {
    configuration: RendererConfiguration<F>,
    image: PrivateImage,
}

impl<F: AsFd> PrivatePreparation<F> {
    /// Allocate and execute private preparation without changing kernel state.
    pub fn prepare(
        device: &Device,
        configuration: RendererConfiguration<F>,
    ) -> Result<Self, PreparationError<RendererConfiguration<F>>> {
        let output = configuration.output();
        let width = std::num::NonZeroU32::new(output.width()).expect("output width is nonzero");
        let height = std::num::NonZeroU32::new(output.height()).expect("output height is nonzero");
        let image = match device.allocate_private(width, height) {
            Ok(image) => image,
            Err(error) => {
                return Err(PreparationError {
                    configuration,
                    error,
                })
            }
        };
        let image = match image.clear_and_wait([0, 0, 0]) {
            Ok(image) => image,
            Err(error) => {
                return Err(PreparationError {
                    configuration,
                    error,
                })
            }
        };
        Ok(Self {
            configuration,
            image,
        })
    }

    /// Finish already-completed private work and recover the configuration.
    pub fn complete(self) -> RendererConfiguration<F> {
        let Self {
            configuration,
            image,
        } = self;
        drop(image);
        configuration
    }
}

/// Failed private preparation with ownership of the renderer configuration.
#[derive(Debug)]
pub struct PreparationError<C> {
    configuration: C,
    error: io::Error,
}

impl<C> PreparationError<C> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (C, io::Error) {
        (self.configuration, self.error)
    }
}
