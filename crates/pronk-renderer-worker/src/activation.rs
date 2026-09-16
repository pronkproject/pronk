//! Private GPU work used to qualify one renderer takeover candidate.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{RegisteredCandidate, SubmittedCandidate};
use pronk_gpu::vulkan::{Device, PrivateImage};

/// A takeover candidate paired with completed work over private GPU storage.
///
/// The image is neither a CastKMS scene source nor a capture destination. It
/// exists only to establish that the selected Vulkan device can allocate and
/// execute work for the candidate's output extent while HOST remains active.
///
/// ```compile_fail
/// use pronk_renderer_worker::PrivateProbe;
/// use std::os::fd::AsFd;
///
/// fn activate_before_submission<F: AsFd>(probe: PrivateProbe<'_, F>) {
///     probe.activate();
/// }
/// ```
#[must_use = "submit the private probe or abort the takeover candidate"]
pub struct PrivateProbe<'renderer, F: AsFd> {
    candidate: RegisteredCandidate<'renderer, F>,
    image: PrivateImage,
}

impl<'renderer, F: AsFd> PrivateProbe<'renderer, F> {
    /// Allocate and execute the private probe without changing kernel state.
    pub fn prepare(
        device: &Device,
        candidate: RegisteredCandidate<'renderer, F>,
    ) -> Result<Self, ProbePreparationError<RegisteredCandidate<'renderer, F>>> {
        let configuration = candidate.configuration();
        let image = match device.allocate_private(configuration.width(), configuration.height()) {
            Ok(image) => image,
            Err(error) => return Err(ProbePreparationError { candidate, error }),
        };
        let image = match image.clear_and_wait([0, 0, 0]) {
            Ok(image) => image,
            Err(error) => return Err(ProbePreparationError { candidate, error }),
        };
        Ok(Self { candidate, image })
    }

    /// Report the already-completed private work to CastKMS.
    pub fn submit(self) -> io::Result<SubmittedCandidate<'renderer, F>> {
        let Self { candidate, image } = self;
        drop(image);
        candidate.submit_private_probe(None)
    }

    /// Abort the candidate without changing the active execution profile.
    pub fn abort(self) -> io::Result<()> {
        let Self { candidate, image } = self;
        drop(image);
        candidate.abort()
    }
}

/// Failed private-probe preparation with ownership of the live candidate.
#[derive(Debug)]
pub struct ProbePreparationError<C> {
    candidate: C,
    error: io::Error,
}

impl<C> ProbePreparationError<C> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (C, io::Error) {
        (self.candidate, self.error)
    }
}
