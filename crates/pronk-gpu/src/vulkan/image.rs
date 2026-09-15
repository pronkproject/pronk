use std::io;
use std::num::NonZeroU32;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;

use super::device::{native, unsupported, Device, DeviceInner};

mod format;
pub use format::PackedFormat;
mod usage;
pub(super) use usage::ImageUse;

/// Allocator-reported single-memory-plane packed layout, without CPU mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLayout {
    pub format: PackedFormat,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub modifier: u64,
    pub offset: u64,
    pub pitch: u64,
    pub allocation_size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageState {
    Uninitialized,
    /// Initialized contents in GENERAL layout with foreign queue ownership.
    Released,
}

/// Dedicated exportable storage. Pixels are undefined until a producer writes it.
///
/// Exporting storage does not authorize publication of uninitialized pixels or
/// change the recipient scope. Keep graphics resources through native completion.
pub struct Image {
    pub(super) device: Arc<DeviceInner>,
    pub(super) raw: vk::Image,
    pub(super) state: ImageState,
    memory: vk::DeviceMemory,
    layout: ImageLayout,
}

impl Device {
    /// Check whether one packed source profile can be imported for reading.
    ///
    /// The query uses the source-only Vulkan usage and DMA-BUF import contract.
    /// It creates no image and grants no access to an allocation. Exact memory
    /// planes, pitch, offset and allocation size still require validation when
    /// a particular source is imported.
    pub fn check_source_image(
        &self,
        format: PackedFormat,
        width: NonZeroU32,
        height: NonZeroU32,
        modifier: u64,
    ) -> io::Result<()> {
        self.check_image(
            format,
            width.get(),
            height.get(),
            modifier,
            ImageUse::ImportedSource,
        )
    }

    /// Allocate BGRA storage with an explicitly selected modifier.
    ///
    /// The caller negotiates the modifier with its intended importer. Capability
    /// checks here qualify only this GPU's single-plane transfer usage.
    pub fn allocate(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
        modifier: u64,
    ) -> io::Result<Image> {
        self.allocate_with_format(PackedFormat::Bgra8, width, height, modifier)
    }

    /// Allocate the requested packed format and modifier without substitution.
    ///
    /// Matching importers must accept the complete reported layout. Native
    /// capability checks qualify this device's single-plane transfer profile,
    /// not a renderer scene or output transport's accepted pixel formats.
    pub fn allocate_with_format(
        &self,
        format: PackedFormat,
        width: NonZeroU32,
        height: NonZeroU32,
        modifier: u64,
    ) -> io::Result<Image> {
        self.check_image(
            format,
            width.get(),
            height.get(),
            modifier,
            ImageUse::OwnedStorage,
        )?;
        let modifiers = [modifier];
        let mut tiling =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let create = vk::ImageCreateInfo::default()
            .push_next(&mut tiling)
            .push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D)
            .format(format.native())
            .extent(vk::Extent3D {
                width: width.get(),
                height: height.get(),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(ImageUse::OwnedStorage.flags())
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: Matching format/modifier/usage capabilities were queried; input
        // chains remain live and no external queue accesses the new image.
        let raw = unsafe { self.inner.raw.create_image(&create, None) }.map_err(native)?;
        let mut image = Image {
            device: Arc::clone(&self.inner),
            raw,
            state: ImageState::Uninitialized,
            memory: vk::DeviceMemory::null(),
            layout: ImageLayout {
                format,
                width,
                height,
                modifier,
                offset: 0,
                pitch: 0,
                allocation_size: 0,
            },
        };
        // SAFETY: Image and physical device belong to the retained live device.
        let requirements = unsafe { self.inner.raw.get_image_memory_requirements(raw) };
        let properties = unsafe {
            self.inner
                .instance()
                .get_physical_device_memory_properties(self.inner.physical)
        };
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
            .ok_or_else(|| unsupported("no device-local memory for the image"))?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(raw);
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let allocation = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated)
            .push_next(&mut export)
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: Dedicated allocation matches the new image's memory requirements.
        image.memory =
            unsafe { self.inner.raw.allocate_memory(&allocation, None) }.map_err(native)?;
        // SAFETY: The allocation is compatible, sufficiently sized and unbound.
        unsafe { self.inner.raw.bind_image_memory(raw, image.memory, 0) }.map_err(native)?;
        let extension = ash::ext::image_drm_format_modifier::Device::new(
            self.inner.instance(),
            &self.inner.raw,
        );
        let mut actual = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: Image was created with modifier tiling and the extension enabled.
        unsafe { extension.get_image_drm_format_modifier_properties(raw, &mut actual) }
            .map_err(native)?;
        if actual.drm_format_modifier != modifier {
            return Err(unsupported(
                "allocator did not select the requested modifier",
            ));
        }
        let subresource =
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT);
        // SAFETY: Capability query restricted the modifier to one memory plane.
        let plane = unsafe {
            self.inner
                .raw
                .get_image_subresource_layout(raw, subresource)
        };
        if plane.row_pitch == 0 || plane.offset >= requirements.size {
            return Err(unsupported(
                "allocator returned an invalid memory-plane layout",
            ));
        }
        image.layout.offset = plane.offset;
        image.layout.pitch = plane.row_pitch;
        image.layout.allocation_size = requirements.size;
        Ok(image)
    }

    pub(super) fn check_image(
        &self,
        packed: PackedFormat,
        width: u32,
        height: u32,
        modifier: u64,
        usage: ImageUse,
    ) -> io::Result<()> {
        let instance = self.inner.instance();
        let physical = self.inner.physical;
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: Live physical device, supported extension and valid output chain.
        unsafe {
            instance.get_physical_device_format_properties2(physical, packed.native(), &mut format)
        };
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut entries);
        let mut format = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: The second query has storage for the enumerated property count.
        unsafe {
            instance.get_physical_device_format_properties2(physical, packed.native(), &mut format)
        };
        if !entries
            .iter()
            .any(|entry| usage.supports_modifier(entry, modifier))
        {
            return Err(unsupported("modifier does not support single-plane blits"));
        }
        let mut tiling = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let query = vk::PhysicalDeviceImageFormatInfo2::default()
            .push_next(&mut tiling)
            .push_next(&mut external)
            .format(packed.native())
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage.flags());
        let mut memory = vk::ExternalImageFormatProperties::default();
        let mut properties = vk::ImageFormatProperties2::default().push_next(&mut memory);
        // SAFETY: Valid query chain for enabled capabilities; outputs are live.
        unsafe {
            instance.get_physical_device_image_format_properties2(physical, &query, &mut properties)
        }
        .map_err(native)?;
        let limits = properties.image_format_properties;
        if width > limits.max_extent.width
            || height > limits.max_extent.height
            || !limits.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
            || !memory
                .external_memory_properties
                .external_memory_features
                .contains(usage.sharing())
        {
            return Err(unsupported(
                "image dimensions or requested DMA-BUF sharing are unsupported",
            ));
        }
        Ok(())
    }
}

impl Image {
    pub(super) fn color_range(&self) -> vk::ImageSubresourceRange {
        vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1)
    }

    pub(super) fn acquire_barrier(
        &self,
        access: vk::AccessFlags,
    ) -> vk::ImageMemoryBarrier<'static> {
        let (layout, from, to) = match self.state {
            ImageState::Uninitialized => (
                vk::ImageLayout::UNDEFINED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
            ),
            ImageState::Released => (
                vk::ImageLayout::GENERAL,
                vk::QUEUE_FAMILY_FOREIGN_EXT,
                self.device.queue_family,
            ),
        };
        vk::ImageMemoryBarrier::default()
            .image(self.raw)
            .old_layout(layout)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(from)
            .dst_queue_family_index(to)
            .dst_access_mask(access)
            .subresource_range(self.color_range())
    }

    pub(super) fn release_barrier(
        &self,
        access: vk::AccessFlags,
    ) -> vk::ImageMemoryBarrier<'static> {
        vk::ImageMemoryBarrier::default()
            .image(self.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(self.device.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(access)
            .subresource_range(self.color_range())
    }

    pub fn layout(&self) -> ImageLayout {
        self.layout
    }

    /// Export storage only, not producer completion or initialized pixels.
    pub fn export(&self) -> io::Result<OwnedFd> {
        let extension =
            ash::khr::external_memory_fd::Device::new(self.device.instance(), &self.device.raw);
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(self.memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: Memory was allocated for DMA-BUF export and is retained by self.
        let fd = unsafe { extension.get_memory_fd(&info) }.map_err(native)?;
        if fd < 0 {
            return Err(io::Error::other("Vulkan returned an invalid DMA-BUF fd"));
        }
        // SAFETY: Successful export transfers ownership of a new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: A submitted job owns its image through native completion.
        // Image destruction precedes freeing dedicated memory; the retained
        // device outlives both operations.
        unsafe {
            self.device.raw.destroy_image(self.raw, None);
            self.device.raw.free_memory(self.memory, None);
        }
    }
}
