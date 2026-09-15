//! Queried optimal-tiling storage without external-memory handle support.

use std::io;
use std::num::NonZeroU32;
use std::sync::Arc;

use ash::vk;

use super::{PrivateImage, FORMAT, USAGE};
use crate::vulkan::device::{native, unsupported};
use crate::vulkan::Device;

impl Device {
    /// Allocate non-exportable RGBA32 floating-point shader and transfer storage.
    ///
    /// The allocation starts uninitialized. This queries the precise format,
    /// optimal tiling, usage and extent; unsupported profiles do not fall back
    /// to CPU memory or another format. Allocation limits remain caller policy.
    pub fn allocate_private(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> io::Result<PrivateImage> {
        let instance = self.inner.instance();
        // SAFETY: The physical device belongs to this retained instance.
        let features =
            unsafe { instance.get_physical_device_format_properties(self.inner.physical, FORMAT) };
        let needed = vk::FormatFeatureFlags::STORAGE_IMAGE
            | vk::FormatFeatureFlags::BLIT_SRC
            | vk::FormatFeatureFlags::BLIT_DST;
        if !features.optimal_tiling_features.contains(needed) {
            return Err(unsupported(
                "private RGBA32 storage or blits are unsupported",
            ));
        }
        // SAFETY: Complete, scalar query for the image profile created below.
        let limits = unsafe {
            instance.get_physical_device_image_format_properties(
                self.inner.physical,
                FORMAT,
                vk::ImageType::TYPE_2D,
                vk::ImageTiling::OPTIMAL,
                USAGE,
                vk::ImageCreateFlags::empty(),
            )
        }
        .map_err(native)?;
        if width.get() > limits.max_extent.width
            || height.get() > limits.max_extent.height
            || width.get() > i32::MAX as u32
            || height.get() > i32::MAX as u32
            || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
        {
            return Err(unsupported("private image extent is unsupported"));
        }
        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width: width.get(),
                height: height.get(),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: The exact profile passed its physical-device capability query.
        let raw = unsafe { self.inner.raw.create_image(&create, None) }.map_err(native)?;
        // SAFETY: Image and physical device are retained and belong together.
        let requirements = unsafe { self.inner.raw.get_image_memory_requirements(raw) };
        let mut image = PrivateImage {
            device: Arc::clone(&self.inner),
            raw,
            memory: vk::DeviceMemory::null(),
            allocation_size: requirements.size,
            width,
            height,
            initialized: false,
        };
        let properties =
            unsafe { instance.get_physical_device_memory_properties(self.inner.physical) };
        let memory_type = properties.memory_types[..properties.memory_type_count as usize]
            .iter()
            .enumerate()
            .find(|(index, ty)| {
                requirements.memory_type_bits & (1 << index) != 0
                    && ty
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .map(|(index, _)| index as u32)
            .ok_or_else(|| unsupported("no device-local memory for private image"))?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(raw);
        let allocation = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated)
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: The dedicated allocation matches image requirements. No
        // export handle types are enabled and no external descriptor is imported.
        image.memory =
            unsafe { self.inner.raw.allocate_memory(&allocation, None) }.map_err(native)?;
        // SAFETY: Compatible, unbound dedicated memory covers the entire image.
        unsafe { self.inner.raw.bind_image_memory(raw, image.memory, 0) }.map_err(native)?;
        Ok(image)
    }
}
