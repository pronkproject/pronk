//! CPU readback is only a test oracle; the producer never maps raw pixels.

use super::*;
use crate::vulkan::Device;
use pronk_dmabuf::Completion;
use std::num::NonZeroU32;

#[test]
fn native_failure_does_not_validate_pixels() {
    assert!(require_success(Completion::Success).is_ok());
    assert!(require_success(Completion::Failed(-5)).is_err());
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn generated_colors_survive_repeated_foreign_handoffs() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let size = NonZeroU32::new(64).unwrap();
    let mut image = device.allocate(size, size, modifier).unwrap();
    for rgb in [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [17, 85, 204],
        [0, 0, 0],
        [255, 255, 255],
    ] {
        let (rendered, completion) = image.clear_waited(rgb).unwrap();
        assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
        let (returned, pixels) = readback(rendered);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[rgb[2], rgb[1], rgb[0], 255]);
        }
        image = returned;
    }
}

fn readback(image: Image) -> (Image, Vec<u8>) {
    let layout = image.layout();
    let size = u64::from(layout.width.get()) * u64::from(layout.height.get()) * 4;
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
