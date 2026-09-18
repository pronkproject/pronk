//! Private-image names bound to one renderer endpoint.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Arc;

use castkms_sys::{
    drm_ioctl_castkms_renderer_register_image, drm_ioctl_castkms_renderer_unregister_image,
    DrmCastkmsRendererRegisterImage, DrmCastkmsRendererUnregisterImage,
};

use crate::{Endpoint, RendererConfiguration, WithdrawnRenderer};

/// One endpoint-local name for renderer-private backing storage.
///
/// The kernel retains the registered DMA-BUFs, but this value does not own the
/// renderer's native image objects. Its caller must keep those objects paired
/// with the name until every bound scene job is released and registration is
/// removed. Dropping the value leaves cleanup to final endpoint close.
#[must_use = "retain the private-image name for job acquisition and explicit removal"]
pub struct RegisteredImage {
    id: NonZeroU64,
    scope: Arc<()>,
}

impl RegisteredImage {
    pub(crate) fn id(&self) -> NonZeroU64 {
        self.id
    }

    pub(crate) fn belongs_to(&self, scope: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.scope, scope)
    }
}

impl<F: AsFd> RendererConfiguration<F> {
    /// Retain private backing under a fresh increasing endpoint-local name.
    pub fn register_image(
        &mut self,
        buffers: &[BorrowedFd<'_>],
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> io::Result<RegisteredImage> {
        if buffers.is_empty() || buffers.len() > 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private image requires one to four backing buffers",
            ));
        }
        let id = self.endpoint.next_image_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::StorageFull,
                "private image identity space is exhausted",
            )
        })?;
        let descriptors = buffers
            .iter()
            .map(|buffer| buffer.as_raw_fd())
            .collect::<Vec<_>>();
        let request = DrmCastkmsRendererRegisterImage {
            image_id: id.get(),
            buffers: descriptors.as_ptr() as u64,
            width: width.get(),
            height: height.get(),
            num_buffers: descriptors.len() as u32,
            ..Default::default()
        };
        // SAFETY: The fixed request and descriptor array remain live throughout
        // the synchronous ioctl. Every descriptor is borrowed for that call.
        unsafe { drm_ioctl_castkms_renderer_register_image(self.as_fd().as_raw_fd(), &request) }?;
        self.endpoint.next_image_id = id.get().checked_add(1).and_then(NonZeroU64::new);
        Ok(RegisteredImage {
            id,
            scope: Arc::clone(&self.endpoint.image_scope),
        })
    }

    /// Remove an idle private-image name without claiming native completion.
    pub fn unregister_image(&mut self, image: RegisteredImage) -> Result<(), UnregisterImageError> {
        unregister(&self.endpoint, image)
    }
}

impl<F: AsFd> WithdrawnRenderer<F> {
    /// Remove a private-image name after source access and backend use end.
    pub fn unregister_image(&mut self, image: RegisteredImage) -> Result<(), UnregisterImageError> {
        unregister(&self.published.configuration.endpoint, image)
    }
}

fn unregister<F: AsFd>(
    endpoint: &Endpoint<F>,
    image: RegisteredImage,
) -> Result<(), UnregisterImageError> {
    if !image.belongs_to(&endpoint.image_scope) {
        return Err(UnregisterImageError {
            image,
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "private image belongs to another renderer",
            ),
        });
    }
    let request = DrmCastkmsRendererUnregisterImage {
        image_id: image.id.get(),
        ..Default::default()
    };
    // SAFETY: The initialized fixed-width request remains live throughout the
    // synchronous ioctl.
    match unsafe {
        drm_ioctl_castkms_renderer_unregister_image(endpoint.as_fd().as_raw_fd(), &request)
    } {
        Ok(_) => Ok(()),
        Err(error) => Err(UnregisterImageError {
            image,
            error: error.into(),
        }),
    }
}

/// Failed private-image removal retaining the name for retry.
pub struct UnregisterImageError {
    image: RegisteredImage,
    error: io::Error,
}

impl UnregisterImageError {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_image(self) -> RegisteredImage {
        self.image
    }
}

impl std::fmt::Debug for UnregisterImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnregisterImageError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for UnregisterImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for UnregisterImageError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names_are_bound_to_one_endpoint_scope() {
        let scope = Arc::new(());
        let image = RegisteredImage {
            id: NonZeroU64::new(1).unwrap(),
            scope: Arc::clone(&scope),
        };
        assert!(image.belongs_to(&scope));
        assert!(!image.belongs_to(&Arc::new(())));
    }
}
