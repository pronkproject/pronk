//! CPU readback helpers used only as native pixel-test oracles.

use std::sync::Arc;

use ash::vk;

use super::submission::Job;
use super::Image;

pub(super) fn readback(image: Image) -> (Image, Vec<u8>) {
    let layout = image.layout();
    let size = u64::from(layout.width.get())
        * u64::from(layout.height.get())
        * u64::from(layout.format.bytes_per_pixel());
    let device = Arc::clone(&image.device);
    let mut job = Job::new(Arc::clone(&device), image).unwrap();
    // SAFETY: This test uses resources from one device, checks each creation,
    // waits for native completion before mapping, and tears down after use.
    // Assertions before teardown conservatively leak test-only native resources.
    unsafe {
        let buffer = device
            .raw
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
            .unwrap();
        let requirements = device.raw.get_buffer_memory_requirements(buffer);
        let properties = device
            .instance()
            .get_physical_device_memory_properties(device.physical);
        let index = properties.memory_types[..properties.memory_type_count as usize]
            .iter()
            .enumerate()
            .find(|(index, ty)| {
                requirements.memory_type_bits & (1 << index) != 0
                    && ty.property_flags.contains(
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )
            })
            .map(|(index, _)| index as u32)
            .unwrap();
        let memory = device
            .raw
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index),
                None,
            )
            .unwrap();
        device.raw.bind_buffer_memory(buffer, memory, 0).unwrap();
        let command = job.command();
        let image = job.resources().raw;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let acquire = vk::ImageMemoryBarrier::default()
            .image(image)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(device.queue_family)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(range);
        device.raw.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[acquire],
        );
        let copy = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: layout.width.get(),
                height: layout.height.get(),
                depth: 1,
            });
        device.raw.cmd_copy_image_to_buffer(
            command,
            image,
            vk::ImageLayout::GENERAL,
            buffer,
            &[copy],
        );
        let host = vk::BufferMemoryBarrier::default()
            .buffer(buffer)
            .size(vk::WHOLE_SIZE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ);
        let release = vk::ImageMemoryBarrier::default()
            .image(image)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(device.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(range);
        device.raw.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS | vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[host],
            &[release],
        );
        job.submit().unwrap();
        let image = job.finish().unwrap();
        let pointer = device
            .raw
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
            .unwrap();
        let pixels = std::slice::from_raw_parts(pointer.cast::<u8>(), size as usize).to_vec();
        device.raw.unmap_memory(memory);
        device.raw.destroy_buffer(buffer, None);
        device.raw.free_memory(memory, None);
        (image, pixels)
    }
}
