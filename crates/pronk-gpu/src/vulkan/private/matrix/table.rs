//! Immutable matrix coefficients uploaded once for an exact native chain.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::color::ColorMatrix;

use crate::vulkan::device::{native, unsupported, DeviceInner};
use crate::vulkan::submission::Job;

pub(super) struct Table {
    device: Arc<DeviceInner>,
    pub(super) buffer: vk::Buffer,
    pub(super) size: u64,
    pub(super) count: u32,
    memory: vk::DeviceMemory,
}

impl Table {
    pub(super) fn new(device: Arc<DeviceInner>, matrices: &[ColorMatrix]) -> io::Result<Self> {
        if matrices.is_empty() {
            return Err(invalid("a native matrix chain must not be empty"));
        }
        let count = u32::try_from(matrices.len())
            .map_err(|_| invalid("native matrix chain length is not representable"))?;
        let size = u64::from(count)
            .checked_mul(12 * 8)
            .ok_or_else(|| invalid("matrix coefficient storage overflowed"))?;
        // SAFETY: The retained owner keeps the physical device and instance live.
        let limit = unsafe {
            device
                .instance()
                .get_physical_device_properties(device.physical)
        }
        .limits
        .max_storage_buffer_range;
        if size > u64::from(limit) {
            return Err(unsupported(
                "matrix chain exceeds native storage-buffer range",
            ));
        }
        let mut table = Self {
            device: Arc::clone(&device),
            buffer: vk::Buffer::null(),
            size,
            count,
            memory: vk::DeviceMemory::null(),
        };
        let create = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: The nonempty buffer fits the checked storage-buffer limit.
        table.buffer = unsafe { device.raw.create_buffer(&create, None) }.map_err(native)?;
        // SAFETY: The new buffer and physical device belong to the same owner.
        let requirements = unsafe { device.raw.get_buffer_memory_requirements(table.buffer) };
        // SAFETY: The physical device belongs to the retained instance.
        let properties = unsafe {
            device
                .instance()
                .get_physical_device_memory_properties(device.physical)
        };
        let index = properties.memory_types[..properties.memory_type_count as usize]
            .iter()
            .enumerate()
            .find(|(index, ty)| {
                requirements.memory_type_bits & (1 << index) != 0
                    && ty
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .map(|(index, _)| index as u32)
            .ok_or_else(|| unsupported("no device-local matrix storage"))?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(table.buffer);
        let allocate = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated)
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: The dedicated allocation matches the owned buffer requirements.
        table.memory = unsafe { device.raw.allocate_memory(&allocate, None) }.map_err(native)?;
        // SAFETY: Compatible, unbound dedicated memory covers the entire buffer.
        unsafe { device.raw.bind_buffer_memory(table.buffer, table.memory, 0) }.map_err(native)?;

        let byte_count = usize::try_from(size)
            .map_err(|_| invalid("matrix coefficient storage is not addressable"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(byte_count)
            .map_err(io::Error::other)?;
        for matrix in matrices {
            for coefficient in matrix.sign_magnitude() {
                bytes.extend_from_slice(&coefficient.to_ne_bytes());
            }
        }
        let mut job = Job::new(device, table)?;
        for (chunk, bytes) in bytes.chunks(65536).enumerate() {
            // SAFETY: Each immediate command copy is word aligned, bounded by
            // 65536 bytes and contained in the owned transfer destination.
            unsafe {
                job.device.raw.cmd_update_buffer(
                    job.command(),
                    job.resources().buffer,
                    (chunk * 65536) as u64,
                    bytes,
                )
            };
        }
        let barrier = vk::BufferMemoryBarrier::default()
            .buffer(job.resources().buffer)
            .size(size)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        // SAFETY: The barrier makes the completed upload visible to later
        // shader reads; the immutable table remains owned through the job.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[barrier],
                &[],
            )
        };
        job.submit()?;
        job.finish()
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: Upload and shader jobs retain the table until retirement.
        unsafe {
            self.device.raw.destroy_buffer(self.buffer, None);
            self.device.raw.free_memory(self.memory, None);
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
