//! Native descriptor and pipeline ownership for one completed private blend.

use std::io::{self, Cursor};
use std::sync::Arc;

use ash::vk;

use super::PrivateImage;
use crate::vulkan::device::{native, DeviceInner};

pub(super) struct Pipeline {
    device: Arc<DeviceInner>,
    views: [vk::ImageView; 2],
    descriptors: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    layout: vk::PipelineLayout,
    shader: vk::ShaderModule,
    pipeline: vk::Pipeline,
}

impl Pipeline {
    // The caller immediately places this owner before both images in Resources.
    pub(super) fn new(source: &PrivateImage, destination: &PrivateImage) -> io::Result<Self> {
        let mut owner = Self {
            device: Arc::clone(&source.device),
            views: [vk::ImageView::null(); 2],
            descriptors: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            layout: vk::PipelineLayout::null(),
            shader: vk::ShaderModule::null(),
            pipeline: vk::Pipeline::null(),
        };
        let raw = &owner.device.raw;
        for (index, image) in [source, destination].iter().enumerate() {
            let view = vk::ImageViewCreateInfo::default()
                .image(image.raw)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(super::super::FORMAT)
                .subresource_range(image.range());
            // SAFETY: The private image supports storage usage and its exact
            // format. The caller keeps it alive beyond the returned view owner.
            owner.views[index] = unsafe { raw.create_image_view(&view, None) }.map_err(native)?;
        }
        let bindings = [0, 1].map(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        let descriptors = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_IMAGE)
            .descriptor_count(2)];
        let pool = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes);
        // SAFETY: Valid scalar limits and live local create-info arrays. Partial
        // creation is owned immediately so later errors destroy native objects.
        unsafe {
            owner.descriptors = raw
                .create_descriptor_set_layout(&descriptors, None)
                .map_err(native)?;
            owner.pool = raw.create_descriptor_pool(&pool, None).map_err(native)?;
        }
        let layouts = [owner.descriptors];
        let allocate = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(owner.pool)
            .set_layouts(&layouts);
        // SAFETY: The pool has space for exactly this one set and its two images.
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
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(8)];
        let layout = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        let words = ash::util::read_spv(&mut Cursor::new(include_bytes!("shader.spv")))?;
        let shader = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: The descriptor set has no pending users. The shader has two
        // formatted storage images and two u32 push constants; no optional
        // device feature is required by its Vulkan 1.1 SPIR-V instructions.
        unsafe {
            raw.update_descriptor_sets(&writes, &[]);
            owner.layout = raw.create_pipeline_layout(&layout, None).map_err(native)?;
            owner.shader = raw.create_shader_module(&shader, None).map_err(native)?;
        }
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(owner.shader)
            .name(c"main");
        let create = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(owner.layout)];
        // SAFETY: The layout matches the shader and every retained object belongs
        // to the same device. The caller has checked the compute queue and limits.
        match unsafe { raw.create_compute_pipelines(vk::PipelineCache::null(), &create, None) } {
            Ok(pipelines) => owner.pipeline = pipelines[0],
            Err((pipelines, error)) => {
                // SAFETY: Even failed pipeline batches may return owned handles.
                for pipeline in pipelines {
                    unsafe { raw.destroy_pipeline(pipeline, None) };
                }
                return Err(native(error));
            }
        }
        Ok(owner)
    }

    /// The caller owns an active recording command buffer on this device and
    /// retains this pipeline and both referenced images through submission.
    pub(super) unsafe fn bind(
        &self,
        raw: &ash::Device,
        command: vk::CommandBuffer,
        parameters: &[u8; 8],
    ) {
        unsafe {
            raw.cmd_bind_pipeline(command, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            raw.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[self.set],
                &[],
            );
            raw.cmd_push_constants(
                command,
                self.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                parameters,
            );
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: The native job retains this entire owner until retirement.
        // On creation failure, no native job references any of these handles.
        unsafe {
            let raw = &self.device.raw;
            raw.destroy_pipeline(self.pipeline, None);
            raw.destroy_shader_module(self.shader, None);
            raw.destroy_pipeline_layout(self.layout, None);
            raw.destroy_descriptor_pool(self.pool, None);
            raw.destroy_descriptor_set_layout(self.descriptors, None);
            for view in self.views {
                raw.destroy_image_view(view, None);
            }
        }
    }
}
