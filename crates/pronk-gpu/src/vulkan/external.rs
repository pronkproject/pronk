//! Common ownership for ordinary DMA-BUF image imports.

use std::io;
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, Access};

use super::device::{external_memory_type, native, unsupported, DeviceInner};
use super::image::ImageUse;
use super::{Device, ImageLayout};

pub(super) struct ExternalImage {
    pub(super) device: Arc<DeviceInner>,
    pub(super) raw: vk::Image,
    pub(super) fd: OwnedFd,
    memory: vk::DeviceMemory,
    pub(super) layout: ImageLayout,
}

impl Device {
    /// Import one externally owned, explicitly described image allocation.
    ///
    /// The caller supplies the access-specific cross-API ownership contract.
    /// This helper checks the common memory layout, DMA-BUF and Vulkan import
    /// requirements but neither waits for dependencies nor authorizes pixels.
    pub(super) unsafe fn import_external_image(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
        usage: ImageUse,
        access: Access,
    ) -> io::Result<ExternalImage> {
        validate_layout(layout)?;
        drop(export_dependencies(fd.as_fd(), access)?);
        let stat = nix::sys::stat::fstat(fd.as_raw_fd())?;
        validate_backing(layout.allocation_size, stat.st_size)?;
        self.check_image(
            layout.format,
            layout.width.get(),
            layout.height.get(),
            layout.modifier,
            usage,
        )?;
        let planes = [vk::SubresourceLayout::default()
            .offset(layout.offset)
            .row_pitch(layout.pitch)];
        let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(layout.modifier)
            .plane_layouts(&planes);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let create = vk::ImageCreateInfo::default()
            .push_next(&mut modifier)
            .push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D)
            .format(layout.format.native())
            .extent(vk::Extent3D {
                width: layout.width.get(),
                height: layout.height.get(),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage.flags())
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: Capability queries and the caller's external-image contract
        // establish valid creation parameters; explicit plane padding is zero.
        let raw = unsafe { self.inner.raw.create_image(&create, None) }.map_err(native)?;
        let mut image = ExternalImage {
            device: Arc::clone(&self.inner),
            raw,
            fd,
            memory: vk::DeviceMemory::null(),
            layout,
        };
        // SAFETY: The new image and physical device belong to this live device.
        let requirements = unsafe { self.inner.raw.get_image_memory_requirements(raw) };
        if requirements.size > layout.allocation_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external allocation is smaller than native requirements",
            ));
        }
        let extension =
            ash::khr::external_memory_fd::Device::new(self.inner.instance(), &self.inner.raw);
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: The retained descriptor is a DMA-BUF with caller-established
        // external-memory compatibility. The output structure is initialized.
        unsafe {
            extension.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                image.fd.as_raw_fd(),
                &mut fd_properties,
            )
        }
        .map_err(native)?;
        // SAFETY: The live physical device is retained by the image owner.
        let properties = unsafe {
            self.inner
                .instance()
                .get_physical_device_memory_properties(self.inner.physical)
        };
        let compatible = requirements.memory_type_bits & fd_properties.memory_type_bits;
        let index = external_memory_type(&properties, compatible)
            .ok_or_else(|| unsupported("no compatible external memory type"))?;
        let imported = image.fd.try_clone()?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(raw);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(imported.as_raw_fd());
        let allocate = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated)
            .push_next(&mut import)
            .allocation_size(layout.allocation_size)
            .memory_type_index(index);
        // SAFETY: Compatible memory type, size and dedicated image. Vulkan takes
        // the duplicate descriptor only if allocation succeeds.
        image.memory =
            unsafe { self.inner.raw.allocate_memory(&allocate, None) }.map_err(native)?;
        let _ = imported.into_raw_fd();
        // SAFETY: Imported memory satisfies this image's checked requirements.
        unsafe { self.inner.raw.bind_image_memory(raw, image.memory, 0) }.map_err(native)?;
        Ok(image)
    }
}

pub(super) fn validate_layout(layout: ImageLayout) -> io::Result<()> {
    if layout.pitch == 0 || layout.offset >= layout.allocation_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid external image memory-plane bounds",
        ));
    }
    if layout.modifier == 0 {
        let row = u64::from(layout.width.get()) * u64::from(layout.format.bytes_per_pixel());
        let end = layout
            .pitch
            .checked_mul(u64::from(layout.height.get() - 1))
            .and_then(|rows| layout.offset.checked_add(rows))
            .and_then(|start| start.checked_add(row));
        if layout.pitch < row || end.is_none_or(|end| end > layout.allocation_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "linear external image rows exceed the allocation",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_backing(allocation_size: u64, backing_size: i64) -> io::Result<()> {
    if u64::try_from(backing_size)
        .ok()
        .is_none_or(|size| size < allocation_size)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "external image allocation exceeds its DMA-BUF backing",
        ));
    }
    Ok(())
}

impl Drop for ExternalImage {
    fn drop(&mut self) {
        // SAFETY: Accepted work retains the complete owner through native
        // completion. The image is destroyed before its imported allocation.
        unsafe {
            self.device.raw.destroy_image(self.raw, None);
            self.device.raw.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::vulkan::PackedFormat;

    #[test]
    fn imported_memory_prefers_local_but_accepts_other_compatible_types() {
        let mut properties = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        properties.memory_types[0].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE;
        properties.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        properties.memory_types[2].property_flags = vk::MemoryPropertyFlags::HOST_COHERENT;
        assert_eq!(external_memory_type(&properties, 0b011), Some(1));
        assert_eq!(external_memory_type(&properties, 0b101), Some(0));
        assert_eq!(external_memory_type(&properties, 0b100), Some(2));
        assert_eq!(external_memory_type(&properties, 0), None);
    }

    #[test]
    fn backing_may_include_native_allocation_rounding() {
        assert!(validate_backing(4096, 4096).is_ok());
        assert!(validate_backing(4096, 8192).is_ok());
        assert!(validate_backing(4096, 4095).is_err());
        assert!(validate_backing(4096, -1).is_err());
    }

    #[test]
    fn linear_row_bounds_follow_the_packed_pixel_size() {
        let layout = ImageLayout {
            format: PackedFormat::Rgb565,
            width: NonZeroU32::new(13).unwrap(),
            height: NonZeroU32::new(7).unwrap(),
            modifier: 0,
            offset: 8,
            pitch: 32,
            allocation_size: 8 + 6 * 32 + 13 * 2,
        };
        assert!(validate_layout(layout).is_ok());
        assert!(validate_layout(ImageLayout {
            allocation_size: layout.allocation_size - 1,
            ..layout
        })
        .is_err());
        assert!(validate_layout(ImageLayout {
            pitch: 25,
            ..layout
        })
        .is_err());
        assert!(validate_layout(ImageLayout {
            format: PackedFormat::Bgra8,
            ..layout
        })
        .is_err());
    }

    #[test]
    fn tiled_layout_bounds_do_not_invent_linear_extents() {
        let mut layout = ImageLayout {
            format: PackedFormat::Bgra8,
            width: NonZeroU32::new(16).unwrap(),
            height: NonZeroU32::new(8).unwrap(),
            modifier: 0,
            offset: 0,
            pitch: 64,
            allocation_size: 512,
        };
        assert!(validate_layout(layout).is_ok());
        layout.allocation_size = 511;
        assert!(validate_layout(layout).is_err());
        layout.modifier = 0x0100_0000_0000_0009;
        assert!(validate_layout(layout).is_ok());
        layout.offset = 511;
        assert!(validate_layout(layout).is_err());
        layout.offset = 0;
        layout.pitch = 0;
        assert!(validate_layout(layout).is_err());
        layout.modifier = 0;
        layout.pitch = u64::MAX;
        assert!(validate_layout(layout).is_err());
    }
}
