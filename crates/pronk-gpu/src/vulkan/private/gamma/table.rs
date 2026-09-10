//! Immutable device-local lookup data uploaded through command-buffer updates.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::color::Lut;

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
    pub(super) fn new(device: Arc<DeviceInner>, entries: &[[u16; 3]]) -> io::Result<Self> {
        Lut::new(entries).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let size = (entries.len() * 16) as u64;
        // SAFETY: The retained owner keeps the physical device and instance live.
        let limits = unsafe {
            device
                .instance()
                .get_physical_device_properties(device.physical)
        }
        .limits;
        if size > u64::from(limits.max_storage_buffer_range) {
            return Err(unsupported(
                "gamma table exceeds native storage-buffer range",
            ));
        }
        let mut table = Self {
            device: Arc::clone(&device),
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            size,
            count: entries.len() as u32,
        };
        let create = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: Nonzero bounded buffer with required transfer and shader usage.
        table.buffer = unsafe { device.raw.create_buffer(&create, None) }.map_err(native)?;
        // SAFETY: The new buffer and physical device belong to the same owner.
        let requirements = unsafe { device.raw.get_buffer_memory_requirements(table.buffer) };
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
            .ok_or_else(|| unsupported("no device-local gamma table memory"))?;
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(table.buffer);
        let allocate = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated)
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: Allocation matches checked buffer requirements and is owned
        // before binding. No host mapping or external-memory handle is enabled.
        table.memory = unsafe { device.raw.allocate_memory(&allocate, None) }.map_err(native)?;
        unsafe { device.raw.bind_buffer_memory(table.buffer, table.memory, 0) }.map_err(native)?;
        let mut job = Job::new(device, table)?;
        // vkCmdUpdateBuffer copies host LUT metadata into command storage. Each
        // chunk is at most 65536 bytes and all offsets and sizes are word aligned.
        for (chunk, entries) in entries.chunks(4096).enumerate() {
            let bytes: Vec<u8> = entries
                .iter()
                .flat_map(|entry| {
                    [
                        u32::from(entry[0]),
                        u32::from(entry[1]),
                        u32::from(entry[2]),
                        0,
                    ]
                    .into_iter()
                    .flat_map(u32::to_ne_bytes)
                })
                .collect();
            // SAFETY: The owned buffer has transfer usage, the checked range
            // fits, and the native command copies the temporary bytes immediately.
            unsafe {
                job.device.raw.cmd_update_buffer(
                    job.command(),
                    job.resources().buffer,
                    (chunk * 65536) as u64,
                    &bytes,
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
        // SAFETY: Establish visibility for subsequent shader reads on the same
        // queue; the immutable table remains owned through upload completion.
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
        // Partial initialization has no users and preserves null handles.
        unsafe {
            self.device.raw.destroy_buffer(self.buffer, None);
            self.device.raw.free_memory(self.memory, None);
        }
    }
}
