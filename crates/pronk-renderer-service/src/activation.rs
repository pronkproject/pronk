//! Checked transition from a configured CastKMS endpoint to publication.

use std::num::NonZeroU32;
use std::os::fd::{AsFd, BorrowedFd};

use castkms_renderer::{
    OperationError, PublicationError, PublishedRenderer, RendererConfiguration,
};
use pronk_gpu::vulkan::Device;

/// Publication authority obtained only after private GPU preparation.
#[must_use = "publish or close the prepared renderer"]
pub(crate) struct PreparedRendererConfiguration<F: AsFd> {
    configuration: RendererConfiguration<F>,
}

impl<F: AsFd> PreparedRendererConfiguration<F> {
    pub(crate) fn prepare(
        configuration: RendererConfiguration<F>,
        device: &Device,
    ) -> Result<Self, OperationError<RendererConfiguration<F>>> {
        let output = configuration.output();
        let width = NonZeroU32::new(output.width()).expect("output width is nonzero");
        let height = NonZeroU32::new(output.height()).expect("output height is nonzero");
        let image = match device.allocate_private(width, height) {
            Ok(image) => image,
            Err(error) => return Err(OperationError::new(configuration, error)),
        };
        let image = match image.clear_and_wait([0, 0, 0]) {
            Ok(image) => image,
            Err(error) => return Err(OperationError::new(configuration, error)),
        };
        drop(image);
        Ok(Self { configuration })
    }

    pub(crate) fn publish(
        self,
        ready_fence: Option<BorrowedFd<'_>>,
    ) -> Result<PublishedRenderer<F>, PublicationError<F>> {
        // SAFETY: Only `prepare` constructs this wrapper, after allocation and
        // completed private GPU work for the configured output.
        unsafe { self.configuration.publish_unchecked(ready_fence) }
    }
}
