//! Private GPU work used to qualify one renderer takeover candidate.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{ActiveRenderer, Renderer, SubmittedCandidate, TakeoverCandidate};
use pronk_gpu::vulkan::{Device, PrivateImage};

/// Activate delegated rendering after qualifying private GPU execution.
pub fn activate_with_private_probe<'renderer, F: AsFd>(
    device: &Device,
    renderer: &'renderer mut Renderer<F>,
) -> Result<ActiveRenderer<'renderer, F>, RendererActivationError> {
    let description = renderer
        .describe()
        .map_err(RendererActivationError::Observe)?;
    let candidate = renderer
        .begin_takeover(description)
        .map_err(RendererActivationError::Reserve)?;
    let probe = PrivateProbe::prepare(device, candidate).map_err(|failure| {
        let (candidate, error) = failure.into_parts();
        drop(candidate);
        RendererActivationError::Prepare(error)
    })?;
    let submitted = probe.submit().map_err(RendererActivationError::Submit)?;
    submitted
        .activate()
        .map_err(|failure| RendererActivationError::Activate(failure.into_error()))
}

/// Stage at which private renderer activation failed.
#[derive(Debug, thiserror::Error)]
pub enum RendererActivationError {
    #[error("observe CastKMS execution before renderer takeover: {0}")]
    Observe(#[source] io::Error),
    #[error("reserve CastKMS renderer takeover: {0}")]
    Reserve(#[source] io::Error),
    #[error("prepare private GPU takeover probe: {0}")]
    Prepare(#[source] io::Error),
    #[error("submit private GPU takeover probe: {0}")]
    Submit(#[source] io::Error),
    #[error("activate CastKMS GPU execution: {0}")]
    Activate(#[source] io::Error),
}

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
    candidate: TakeoverCandidate<'renderer, F>,
    image: PrivateImage,
}

impl<'renderer, F: AsFd> PrivateProbe<'renderer, F> {
    /// Allocate and execute the private probe without changing kernel state.
    pub fn prepare(
        device: &Device,
        candidate: TakeoverCandidate<'renderer, F>,
    ) -> Result<Self, ProbePreparationError<TakeoverCandidate<'renderer, F>>> {
        let configuration = candidate.configuration();
        let image = match device.allocate_private(configuration.width(), configuration.height()) {
            Ok(image) => image,
            Err(error) => return Err(ProbePreparationError { candidate, error }),
        };
        let image = match image.clear_waited([0, 0, 0]) {
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
