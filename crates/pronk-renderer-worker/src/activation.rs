//! Private GPU work used to qualify one renderer offer.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{ProbedRenderer, RendererDraft};
use pronk_gpu::vulkan::{Device, PrivateImage};

/// A renderer draft paired with completed work over private GPU storage.
///
/// The image is neither a CastKMS scene source nor a capture destination. It
/// exists only to establish that the selected Vulkan device can allocate and
/// execute work for the offer's output extent before publication.
///
/// ```compile_fail
/// use pronk_renderer_worker::PrivateProbe;
/// use std::os::fd::AsFd;
///
/// fn publish_before_submission<F: AsFd>(probe: PrivateProbe<F>) {
///     probe.publish();
/// }
/// ```
#[must_use = "submit the private probe or close the renderer draft"]
pub struct PrivateProbe<F: AsFd> {
    draft: RendererDraft<F>,
    image: PrivateImage,
}

impl<F: AsFd> PrivateProbe<F> {
    /// Allocate and execute the private probe without changing kernel state.
    pub fn prepare(
        device: &Device,
        draft: RendererDraft<F>,
    ) -> Result<Self, ProbePreparationError<RendererDraft<F>>> {
        let output = draft.output();
        let width = std::num::NonZeroU32::new(output.width()).expect("output width is nonzero");
        let height = std::num::NonZeroU32::new(output.height()).expect("output height is nonzero");
        let image = match device.allocate_private(width, height) {
            Ok(image) => image,
            Err(error) => return Err(ProbePreparationError { draft, error }),
        };
        let image = match image.clear_and_wait([0, 0, 0]) {
            Ok(image) => image,
            Err(error) => return Err(ProbePreparationError { draft, error }),
        };
        Ok(Self { draft, image })
    }

    /// Report the already-completed private work to CastKMS.
    pub fn submit(
        self,
    ) -> Result<ProbedRenderer<F>, castkms_renderer::OperationError<RendererDraft<F>>> {
        let Self { draft, image } = self;
        drop(image);
        draft.submit_probe(None)
    }
}

/// Failed private-probe preparation with ownership of the renderer draft.
#[derive(Debug)]
pub struct ProbePreparationError<C> {
    draft: C,
    error: io::Error,
}

impl<C> ProbePreparationError<C> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (C, io::Error) {
        (self.draft, self.error)
    }
}
