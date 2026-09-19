//! Delivery of completed private images into kernel-issued recipient storage.

use std::io;
use std::num::NonZeroU32;
use std::os::fd::AsFd;

use pronk_gpu::vulkan::{DestinationImage, ImageLayout};

use crate::scene_image::{CopiedRecipientImage, RenderedFrame};
use crate::source::packed_format;
use crate::SceneReader;

/// Result of one nonblocking recipient-delivery attempt.
#[must_use = "reuse the completed private frame or retain it for later delivery"]
pub enum DeliveryAttempt {
    NoRecipient(RenderedFrame),
    Delivered(RenderedFrame),
    /// No recipient access began because the private image belongs to old output content.
    Stale(RenderedFrame),
}

/// Failed delivery, retaining a reusable frame when no access began.
pub struct DeliveryError {
    cause: io::Error,
    frame: Option<Box<RenderedFrame>>,
}

impl std::fmt::Debug for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeliveryError")
            .field("has_reusable_frame", &self.frame.is_some())
            .field("cause", &self.cause)
            .finish()
    }
}

impl DeliveryError {
    fn before_access(cause: io::Error, frame: RenderedFrame) -> Self {
        Self {
            cause,
            frame: Some(Box::new(frame)),
        }
    }

    fn after_access(cause: io::Error) -> Self {
        Self { cause, frame: None }
    }

    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (Option<RenderedFrame>, io::Error) {
        (self.frame.map(|frame| *frame), self.cause)
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "deliver rendered frame: {}", self.cause)
    }
}

impl std::error::Error for DeliveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl<F: AsFd> SceneReader<F> {
    /// Try to deliver one completed scene without reacquiring its KMS sources.
    ///
    /// Recipient reuse waits and native copying may block. Run this operation
    /// on the renderer's blocking graphics worker.
    pub fn try_deliver(&mut self, frame: RenderedFrame) -> Result<DeliveryAttempt, DeliveryError> {
        let device = self.device().clone();
        let job = match self.output.try_acquire(frame.scene.registration()) {
            Ok(Some(job)) => job,
            Ok(None) => return Ok(DeliveryAttempt::NoRecipient(frame)),
            Err(cause) if cause.raw_os_error() == Some(nix::libc::ESTALE) => {
                return Ok(DeliveryAttempt::Stale(frame));
            }
            Err(cause) => return Err(DeliveryError::before_access(cause, frame)),
        };
        let destination = match import_destination(&device, job.destination()) {
            Ok(destination) => destination,
            Err(cause) => {
                return match job.release_without_access() {
                    Ok(()) => Err(DeliveryError::before_access(cause, frame)),
                    Err(error) => {
                        let (_, release) = error.into_parts();
                        drop(frame);
                        Err(DeliveryError::after_access(io::Error::new(
                            release.kind(),
                            format!("{cause}; release unused recipient: {release}"),
                        )))
                    }
                };
            }
        };
        let RenderedFrame { private, scene } = frame;
        let CopiedRecipientImage {
            scene,
            destination,
            completion,
        } = scene
            .copy_to_recipient(destination)
            .map_err(DeliveryError::after_access)?;
        if let Err(error) = job.release_submitted(completion.as_fd()) {
            let (_, cause) = error.into_parts();
            drop((private, scene, destination, completion));
            return Err(DeliveryError::after_access(cause));
        }
        drop((destination, completion));
        Ok(DeliveryAttempt::Delivered(RenderedFrame { private, scene }))
    }
}

fn import_destination(
    device: &pronk_gpu::vulkan::Device,
    destination: &castkms_renderer::RecipientImage,
) -> io::Result<DestinationImage> {
    let extent = destination.extent();
    let layout = ImageLayout {
        format: packed_format(destination.format())?,
        width: NonZeroU32::new(extent.width()).expect("recipient width is nonzero"),
        height: NonZeroU32::new(extent.height()).expect("recipient height is nonzero"),
        modifier: destination.modifier(),
        offset: destination.offset(),
        pitch: u64::from(destination.pitch().get()),
        allocation_size: destination.allocation_size(),
    };
    let fd = destination.as_fd().try_clone_to_owned()?;
    // SAFETY: The CastKMS output job retains the exact checked recipient
    // allocation and excludes competing use until its terminal release.
    unsafe { device.import_destination(fd, layout) }
}
