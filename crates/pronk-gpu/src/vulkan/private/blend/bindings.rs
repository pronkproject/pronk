//! Per-operation image views and descriptors, never shared between submissions.

use std::io;
use std::sync::Arc;

use ash::vk;

use super::{geometry::PARAMETER_SIZE, pipeline::Program, PrivateImage};
use crate::vulkan::device::native;

pub(super) struct Bindings {
    program: Arc<Program>,
    views: [vk::ImageView; 2],
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
}

impl Bindings {
    // The caller keeps this owner before its two referenced images in Resources.
    pub(super) fn new(
        program: Arc<Program>,
        source: &PrivateImage,
        destination: &PrivateImage,
    ) -> io::Result<Self> {
        let mut owner = Self {
            program,
            views: [vk::ImageView::null(); 2],
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
        };
        let raw = &owner.program.device.raw;
        for (index, image) in [source, destination].iter().enumerate() {
            let view = vk::ImageViewCreateInfo::default()
                .image(image.raw)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(super::super::FORMAT)
                .subresource_range(image.range());
            // SAFETY: The caller checked device identity. Private images support
            // the exact storage format and outlive the returned view owner.
            owner.views[index] = unsafe { raw.create_image_view(&view, None) }.map_err(native)?;
        }
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_IMAGE)
            .descriptor_count(2)];
        let pool = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes);
        // SAFETY: Valid descriptor counts; partial creation is immediately owned.
        owner.pool = unsafe { raw.create_descriptor_pool(&pool, None) }.map_err(native)?;
        let layouts = [owner.program.descriptors];
        let allocate = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(owner.pool)
            .set_layouts(&layouts);
        // SAFETY: The new pool has space for exactly this set and its two images.
        owner.set = unsafe { raw.allocate_descriptor_sets(&allocate) }.map_err(native)?[0];
        let images = owner.views.map(|view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::GENERAL)]
        });
        let writes = [0usize, 1].map(|index| {
            vk::WriteDescriptorSet::default()
                .dst_set(owner.set)
                .dst_binding(index as u32)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&images[index])
        });
        // SAFETY: No pending users exist; bindings and image layouts match the
        // retained immutable program. Each submission has a distinct set.
        unsafe { raw.update_descriptor_sets(&writes, &[]) };
        Ok(owner)
    }

    /// The caller owns an active command buffer on this device and retains these
    /// bindings and their two images through submission retirement.
    pub(super) unsafe fn bind(
        &self,
        command: vk::CommandBuffer,
        parameters: &[u8; PARAMETER_SIZE],
    ) {
        let raw = &self.program.device.raw;
        unsafe {
            raw.cmd_bind_pipeline(
                command,
                vk::PipelineBindPoint::COMPUTE,
                self.program.pipeline,
            );
            raw.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::COMPUTE,
                self.program.layout,
                0,
                &[self.set],
                &[],
            );
            raw.cmd_push_constants(
                command,
                self.program.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                parameters,
            );
        }
    }
}

impl Drop for Bindings {
    fn drop(&mut self) {
        // SAFETY: The native job retains this entire owner until retirement,
        // and destroys it before the images. Partial creation has no users.
        unsafe {
            let raw = &self.program.device.raw;
            raw.destroy_descriptor_pool(self.pool, None);
            for view in self.views {
                raw.destroy_image_view(view, None);
            }
        }
    }
}
