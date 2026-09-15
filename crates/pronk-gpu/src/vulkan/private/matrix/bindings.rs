use std::io;
use std::sync::Arc;

use ash::vk;

use super::pipeline::{Program, PARAMETER_SIZE};
use super::PrivateImage;
use crate::vulkan::device::native;

pub(super) struct Bindings {
    program: Arc<Program>,
    view: vk::ImageView,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
}

impl Bindings {
    pub(super) fn new(program: Arc<Program>, image: &PrivateImage) -> io::Result<Self> {
        let mut owner = Self {
            program,
            view: vk::ImageView::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
        };
        let raw = &owner.program.device.raw;
        let view = vk::ImageViewCreateInfo::default()
            .image(image.raw)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(super::super::FORMAT)
            .subresource_range(image.range());
        // SAFETY: The caller checked device identity and retains the image.
        owner.view = unsafe { raw.create_image_view(&view, None) }.map_err(native)?;
        let sizes = [
            vk::DescriptorType::STORAGE_IMAGE,
            vk::DescriptorType::STORAGE_BUFFER,
        ]
        .map(|ty| vk::DescriptorPoolSize::default().ty(ty).descriptor_count(1));
        let pool = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes);
        // SAFETY: Pool counts cover exactly one matching descriptor set.
        owner.pool = unsafe { raw.create_descriptor_pool(&pool, None) }.map_err(native)?;
        let layouts = [owner.program.descriptors];
        let allocate = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(owner.pool)
            .set_layouts(&layouts);
        // SAFETY: The retained pool and immutable matching layout are live.
        owner.set = unsafe { raw.allocate_descriptor_sets(&allocate) }.map_err(native)?[0];
        let images = [vk::DescriptorImageInfo::default()
            .image_view(owner.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let buffers = [vk::DescriptorBufferInfo::default()
            .buffer(owner.program.matrices.buffer)
            .range(owner.program.matrices.size)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(owner.set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&images),
            vk::WriteDescriptorSet::default()
                .dst_set(owner.set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&buffers),
        ];
        // SAFETY: Descriptors retain the complete image view and immutable
        // matrix storage with ranges matching the shader interface.
        unsafe { raw.update_descriptor_sets(&writes, &[]) };
        Ok(owner)
    }

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
        // SAFETY: Native retirement precedes the view and pool destruction.
        unsafe {
            let raw = &self.program.device.raw;
            raw.destroy_descriptor_pool(self.pool, None);
            raw.destroy_image_view(self.view, None);
        }
    }
}
