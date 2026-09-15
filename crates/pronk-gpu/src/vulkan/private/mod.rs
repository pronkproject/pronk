//! Non-exportable intermediate storage, separate from shared output allocations.

use std::num::NonZeroU32;
use std::sync::Arc;

use ash::vk;

use super::device::DeviceInner;

mod allocation;
mod blend;
mod clear;
mod color;
mod gamma;
mod matrix;
mod output;
mod profile;
mod scene;
mod source;
mod transfer;

pub use blend::{BlendedImages, Blender};
pub use color::{ColorPipelineProgram, OutputColorProgram};
pub use gamma::Gamma;
pub use matrix::OutputMatrix;
pub use output::PrivateCopy;
pub use profile::{LayerRequirements, SceneRequirements, SourceRequirements};
pub use scene::{ComposedScene, PrivateLayer};
pub use source::PendingPrivateRead;

const FORMAT: vk::Format = vk::Format::R32G32B32A32_SFLOAT;
const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::TRANSFER_SRC.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw()
        | vk::ImageUsageFlags::STORAGE.as_raw(),
);

/// Exclusively owned, non-exportable floating-point RGBA intermediate storage.
///
/// Only this Vulkan device accesses the allocation. Operations consume its
/// owner until native completion; no DMA-BUF descriptor, clone or pixel mapping
/// is exposed. This is private rendering storage, not a capture destination.
///
/// ```compile_fail
/// use pronk_gpu::vulkan::PrivateImage;
/// fn share_private_pixels(image: PrivateImage) {
///     image.export();
/// }
/// ```
pub struct PrivateImage {
    device: Arc<DeviceInner>,
    raw: vk::Image,
    memory: vk::DeviceMemory,
    allocation_size: u64,
    width: NonZeroU32,
    height: NonZeroU32,
    initialized: bool,
}

impl PrivateImage {
    pub fn extent(&self) -> (NonZeroU32, NonZeroU32) {
        (self.width, self.height)
    }

    /// Whether this image belongs to the supplied logical device instance.
    pub fn is_owned_by(&self, device: &super::Device) -> bool {
        Arc::ptr_eq(&self.device, &device.inner)
    }

    /// Number of device-memory bytes dedicated to the image.
    pub fn allocation_size(&self) -> u64 {
        self.allocation_size
    }

    fn range(&self) -> vk::ImageSubresourceRange {
        vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1)
    }

    fn barrier(&self, access: vk::AccessFlags) -> vk::ImageMemoryBarrier<'static> {
        vk::ImageMemoryBarrier::default()
            .image(self.raw)
            .old_layout(if self.initialized {
                vk::ImageLayout::GENERAL
            } else {
                vk::ImageLayout::UNDEFINED
            })
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(if self.initialized {
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
            } else {
                vk::AccessFlags::empty()
            })
            .dst_access_mask(access)
            .subresource_range(self.range())
    }
}

impl Drop for PrivateImage {
    fn drop(&mut self) {
        // SAFETY: Accepted jobs retain the unique image owner until native
        // retirement. Partial creation leaves null memory; image precedes memory.
        unsafe {
            self.device.raw.destroy_image(self.raw, None);
            self.device.raw.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
mod tests;
