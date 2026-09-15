//! Native source imports are distinct from executor-owned writable allocations.

use std::io;
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, Access, SyncFile};

use super::device::{native, DeviceInner};
use super::image::USAGE;
use super::{Device, ImageLayout};

/// One imported source use with its explicit producer dependency retained.
///
/// This type has no clear, output-publication or writable-image conversion API.
/// It does not represent a capture grant or revoke other copies of the source fd.
///
/// ```compile_fail
/// use pronk_gpu::vulkan::SourceImage;
/// fn overwrite(source: SourceImage) {
///     source.clear_waited([0, 0, 0]);
/// }
/// ```
pub struct SourceImage {
    pub(super) device: Arc<DeviceInner>,
    pub(super) raw: vk::Image,
    pub(super) fd: OwnedFd,
    pub(super) producer: Option<SyncFile>,
    memory: vk::DeviceMemory,
    layout: ImageLayout,
}

impl Device {
    /// Import a single-plane packed source through the ordinary Vulkan driver.
    ///
    /// Both descriptors are consumed on success and failure. The explicit
    /// producer dependency remains separate from a later reservation snapshot.
    /// Import performs no source reading and does not wait for the producer.
    ///
    /// # Safety
    ///
    /// The descriptor and metadata must identify a compatible image allocation
    /// on this physical GPU, satisfying Vulkan external-memory requirements for
    /// the reported format, dimensions and transfer usage. The supplied submitted
    /// fence must cover all producer writes and release in GENERAL layout with
    /// foreign queue ownership. Failed producer completion is allowed as input,
    /// but must not authorize reading invalid pixels. Native bounds checks
    /// cannot establish those cross-API facts. The caller must hold source-read
    /// authority and prevent pixel reuse throughout the eventual read operation.
    pub unsafe fn import_source(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
        producer: SyncFile,
    ) -> io::Result<SourceImage> {
        // SAFETY: The caller supplies the external-image contract documented
        // above; the producer record is retained by the imported source.
        unsafe { self.import_source_with_completion(fd, layout, Some(producer)) }
    }

    /// Import a source whose captured producer work has already completed.
    ///
    /// Reservation dependencies discovered at submission time remain mandatory.
    /// The operation only omits a separate captured producer wait.
    ///
    /// # Safety
    ///
    /// The descriptor and metadata have the external-image obligations of
    /// [`Self::import_source`]. In addition, every producer dependency that the
    /// source owner captured before issuing the descriptor must have completed
    /// successfully. A missing record alone does not establish that fact.
    pub unsafe fn import_ready_source(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
    ) -> io::Result<SourceImage> {
        // SAFETY: The caller supplies both documented external contracts.
        unsafe { self.import_source_with_completion(fd, layout, None) }
    }

    unsafe fn import_source_with_completion(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
        producer: Option<SyncFile>,
    ) -> io::Result<SourceImage> {
        validate_layout(layout)?;
        // The native ioctl establishes that this is a DMA-BUF before Vulkan sees
        // it. It does not replace the explicit producer's completion status.
        drop(export_dependencies(fd.as_fd(), Access::Read)?);
        let stat = nix::sys::stat::fstat(fd.as_raw_fd())?;
        validate_backing(layout.allocation_size, stat.st_size)?;
        self.check_image(
            layout.format,
            layout.width.get(),
            layout.height.get(),
            layout.modifier,
            vk::ExternalMemoryFeatureFlags::IMPORTABLE,
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
            .usage(USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: Capability queries and the caller's external-image contract
        // establish valid creation parameters; explicit plane padding stays zero.
        let raw = unsafe { self.inner.raw.create_image(&create, None) }.map_err(native)?;
        let mut source = SourceImage {
            device: Arc::clone(&self.inner),
            raw,
            fd,
            producer,
            memory: vk::DeviceMemory::null(),
            layout,
        };
        // SAFETY: The new image and physical device belong to this live device.
        let requirements = unsafe { self.inner.raw.get_image_memory_requirements(raw) };
        if requirements.size > layout.allocation_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source allocation is smaller than native requirements",
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
                source.fd.as_raw_fd(),
                &mut fd_properties,
            )
        }
        .map_err(native)?;
        // SAFETY: Live physical device retained by the source owner.
        let properties = unsafe {
            self.inner
                .instance()
                .get_physical_device_memory_properties(self.inner.physical)
        };
        let compatible = requirements.memory_type_bits & fd_properties.memory_type_bits;
        let index = properties.memory_types[..properties.memory_type_count as usize]
            .iter()
            .enumerate()
            .find(|(index, ty)| {
                compatible & (1 << index) != 0
                    && ty
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .map(|(index, _)| index as u32)
            .ok_or_else(|| {
                super::device::unsupported("no compatible device-local source memory")
            })?;
        let imported = source.fd.try_clone()?;
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
        source.memory =
            unsafe { self.inner.raw.allocate_memory(&allocate, None) }.map_err(native)?;
        let _ = imported.into_raw_fd();
        // SAFETY: Imported memory satisfies this image's checked requirements.
        unsafe { self.inner.raw.bind_image_memory(raw, source.memory, 0) }.map_err(native)?;
        Ok(source)
    }
}

impl SourceImage {
    pub fn layout(&self) -> ImageLayout {
        self.layout
    }

    pub(super) fn wait_for_producer(&self) -> io::Result<()> {
        let Some(producer) = &self.producer else {
            return Ok(());
        };
        super::submission::require_success(
            SyncFile::from_fd(producer.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )
    }
}

fn validate_layout(layout: ImageLayout) -> io::Result<()> {
    if layout.pitch == 0 || layout.offset >= layout.allocation_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid source memory-plane bounds",
        ));
    }
    if layout.modifier == 0 {
        let row = u64::from(layout.width.get()) * 4;
        let end = layout
            .pitch
            .checked_mul(u64::from(layout.height.get() - 1))
            .and_then(|rows| layout.offset.checked_add(rows))
            .and_then(|start| start.checked_add(row));
        if layout.pitch < row || end.is_none_or(|end| end > layout.allocation_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "linear source rows exceed the allocation",
            ));
        }
    }
    Ok(())
}

fn validate_backing(allocation_size: u64, backing_size: i64) -> io::Result<()> {
    if u64::try_from(backing_size)
        .ok()
        .is_none_or(|size| size < allocation_size)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source allocation exceeds its DMA-BUF backing",
        ));
    }
    Ok(())
}

impl Drop for SourceImage {
    fn drop(&mut self) {
        // SAFETY: Accepted source reads retain the entire owner in their native
        // job. The image is destroyed before its imported allocation and fd.
        unsafe {
            self.device.raw.destroy_image(self.raw, None);
            self.device.raw.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    #[test]
    fn backing_may_include_native_allocation_rounding() {
        assert!(validate_backing(4096, 4096).is_ok());
        assert!(validate_backing(4096, 8192).is_ok());
        assert!(validate_backing(4096, 4095).is_err());
        assert!(validate_backing(4096, -1).is_err());
    }

    #[test]
    fn source_layout_bounds_do_not_invent_tiled_extents() {
        let mut layout = ImageLayout {
            format: super::super::PackedFormat::Bgra8,
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
