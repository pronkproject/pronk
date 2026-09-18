//! Registered packed storage for completed renderer scene jobs.

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::AsFd;
use std::sync::Arc;

use castkms_renderer::{RegisteredImage, RendererConfiguration};
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{
    DestinationCopy, DestinationImage, Device, Image, ImageLayout, PrivateCopy,
};

use crate::pool::{MAX_PRIVATE_BUFFERS, MAX_PRIVATE_POOL_BYTES};
use crate::{PrivateBuffer, PrivateFrame};

/// One packed image kept private to the renderer and named by CastKMS.
///
/// The name and native image remain inseparable so a scene can only be
/// dequeued into the allocation that userspace subsequently writes.
pub(crate) struct SceneImage {
    pool: Arc<()>,
    registration: RegisteredImage,
    image: Image,
}

impl SceneImage {
    pub(crate) fn registration(&self) -> &RegisteredImage {
        &self.registration
    }

    pub(crate) fn write(self, frame: PrivateFrame) -> io::Result<CompletedSceneImage> {
        let PrivateFrame {
            buffer:
                PrivateBuffer {
                    identity,
                    image: source,
                },
            content_serial,
            alpha,
        } = frame;
        let PrivateCopy {
            source,
            destination,
            completion,
        } = source.copy_into_and_wait(self.image)?;
        Ok(CompletedSceneImage {
            frame: RenderedFrame {
                private: PrivateFrame {
                    buffer: PrivateBuffer {
                        identity,
                        image: source,
                    },
                    content_serial,
                    alpha,
                },
                scene: SceneImage {
                    pool: self.pool,
                    registration: self.registration,
                    image: destination,
                },
            },
            completion,
        })
    }

    pub(crate) fn copy_to_recipient(
        self,
        destination: DestinationImage,
    ) -> io::Result<CopiedRecipientImage> {
        let DestinationCopy {
            source,
            destination,
            completion,
        } = destination.copy_from_and_wait(self.image)?;
        Ok(CopiedRecipientImage {
            scene: SceneImage {
                pool: self.pool,
                registration: self.registration,
                image: source,
            },
            destination,
            completion,
        })
    }
}

/// A private float image and its packed CastKMS scene image.
#[must_use = "copy the rendered scene to output or return both private owners"]
pub struct RenderedFrame {
    pub(crate) private: PrivateFrame,
    pub(crate) scene: SceneImage,
}

pub(crate) struct CompletedSceneImage {
    pub(crate) frame: RenderedFrame,
    pub(crate) completion: SyncFile,
}

pub(crate) struct CopiedRecipientImage {
    pub(crate) scene: SceneImage,
    pub(crate) destination: DestinationImage,
    pub(crate) completion: SyncFile,
}

/// Bounded private images registered for one renderer configuration.
pub struct RegisteredSceneImages {
    identity: Arc<()>,
    images: Vec<SceneImage>,
    layout: ImageLayout,
    capacity: NonZeroUsize,
}

/// Packed private storage allocated while a renderer configuration remains unpublished.
#[must_use = "register the prepared images before publishing their renderer"]
pub struct PreparedSceneImages {
    images: Vec<Image>,
    layout: ImageLayout,
    capacity: NonZeroUsize,
}

impl PreparedSceneImages {
    pub fn new(
        device: &Device,
        width: NonZeroU32,
        height: NonZeroU32,
        modifier: u64,
        capacity: NonZeroUsize,
    ) -> io::Result<Self> {
        validate_capacity(capacity)?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(capacity.get())
            .map_err(io::Error::other)?;
        let mut allocated_bytes = 0;
        for _ in 0..capacity.get() {
            let image = device.allocate(width, height, modifier)?;
            allocated_bytes = account_allocation(allocated_bytes, image.layout().allocation_size)?;
            images.push(image);
        }
        let layout = images
            .first()
            .expect("nonzero scene image capacity")
            .layout();
        if images.iter().any(|image| image.layout() != layout) {
            return Err(invalid(
                "scene image allocator returned inconsistent layouts",
            ));
        }
        Ok(Self {
            images,
            layout,
            capacity,
        })
    }

    pub fn register<F: AsFd>(
        self,
        renderer: &mut RendererConfiguration<F>,
    ) -> io::Result<RegisteredSceneImages> {
        let Self {
            images: prepared,
            layout,
            capacity,
        } = self;
        let identity = Arc::new(());
        let mut images = Vec::new();
        images
            .try_reserve_exact(prepared.len())
            .map_err(io::Error::other)?;
        for image in prepared {
            let descriptor = match image.export() {
                Ok(descriptor) => descriptor,
                Err(error) => return Err(cleanup(renderer, images, error)),
            };
            let registration =
                match renderer.register_image(&[descriptor.as_fd()], layout.width, layout.height) {
                    Ok(registration) => registration,
                    Err(error) => return Err(cleanup(renderer, images, error)),
                };
            images.push(SceneImage {
                pool: Arc::clone(&identity),
                registration,
                image,
            });
        }
        Ok(RegisteredSceneImages {
            identity,
            images,
            layout,
            capacity,
        })
    }
}

fn validate_capacity(capacity: NonZeroUsize) -> io::Result<()> {
    if capacity.get() > MAX_PRIVATE_BUFFERS {
        return Err(invalid("scene image pool exceeds its buffer limit"));
    }
    Ok(())
}

fn account_allocation(total: u64, bytes: u64) -> io::Result<u64> {
    let total = total
        .checked_add(bytes)
        .ok_or_else(|| invalid("scene image pool allocation size overflowed"))?;
    if total > MAX_PRIVATE_POOL_BYTES {
        return Err(invalid("scene image pool exceeds its allocation limit"));
    }
    Ok(total)
}

impl RegisteredSceneImages {
    pub(crate) fn available(&self) -> usize {
        self.images.len()
    }

    pub(crate) fn take(&mut self) -> Option<SceneImage> {
        self.images.pop()
    }

    pub(crate) fn restore(&mut self, image: SceneImage) -> io::Result<()> {
        if !self.accepts(&image) {
            return Err(invalid("scene image does not belong to this pool"));
        }
        self.restore_validated(image);
        Ok(())
    }

    pub(crate) fn accepts(&self, image: &SceneImage) -> bool {
        Arc::ptr_eq(&image.pool, &self.identity)
            && image.image.layout() == self.layout
            && self.images.len() < self.capacity.get()
    }

    pub(crate) fn restore_validated(&mut self, image: SceneImage) {
        self.images.push(image);
    }
}

fn cleanup<F: AsFd>(
    renderer: &mut RendererConfiguration<F>,
    images: Vec<SceneImage>,
    primary: io::Error,
) -> io::Error {
    let mut cleanup = None;
    for image in images {
        if let Err(error) = renderer.unregister_image(image.registration) {
            cleanup.get_or_insert_with(|| error.to_string());
        }
    }
    match cleanup {
        Some(cleanup) => io::Error::new(
            primary.kind(),
            format!("{primary}; unregister scene image: {cleanup}"),
        ),
        None => primary,
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_image_pool_rejects_excess_capacity() {
        let capacity = NonZeroUsize::new(MAX_PRIVATE_BUFFERS + 1).unwrap();
        assert_eq!(
            validate_capacity(capacity).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn scene_image_pool_rejects_excess_native_allocation() {
        assert_eq!(
            account_allocation(MAX_PRIVATE_POOL_BYTES, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            account_allocation(MAX_PRIVATE_POOL_BYTES - 1, 1).unwrap(),
            MAX_PRIVATE_POOL_BYTES
        );
    }
}
