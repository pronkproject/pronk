//! Immutable native program shared by independently owned blend submissions.

use std::io::{self, Cursor};
use std::sync::Arc;

use ash::vk;

use super::geometry::PARAMETER_SIZE;
use crate::vulkan::device::{native, unsupported, DeviceInner};

pub(super) struct Program {
    pub(super) device: Arc<DeviceInner>,
    pub(super) descriptors: vk::DescriptorSetLayout,
    pub(super) layout: vk::PipelineLayout,
    pub(super) pipeline: vk::Pipeline,
    pub(super) max_groups: [u32; 2],
    shader: vk::ShaderModule,
}

impl Program {
    pub(super) fn new(device: Arc<DeviceInner>) -> io::Result<Self> {
        // SAFETY: The owner retains the physical device and its instance.
        let queues = unsafe {
            device
                .instance()
                .get_physical_device_queue_family_properties(device.physical)
        };
        let limits = unsafe {
            device
                .instance()
                .get_physical_device_properties(device.physical)
        }
        .limits;
        if !queues[device.queue_family as usize]
            .queue_flags
            .contains(vk::QueueFlags::COMPUTE)
            || limits.max_compute_work_group_size[0] < 8
            || limits.max_compute_work_group_size[1] < 8
            || limits.max_compute_work_group_invocations < 64
        {
            return Err(unsupported("private blend compute program is unsupported"));
        }
        let mut owner = Self {
            device,
            descriptors: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            shader: vk::ShaderModule::null(),
            pipeline: vk::Pipeline::null(),
            max_groups: [
                limits.max_compute_work_group_count[0],
                limits.max_compute_work_group_count[1],
            ],
        };
        let raw = &owner.device.raw;
        let bindings = [0, 1].map(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        let descriptors = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: Valid scalar limits and live local create-info arrays. Partial
        // creation is owned immediately so later errors destroy native objects.
        unsafe {
            owner.descriptors = raw
                .create_descriptor_set_layout(&descriptors, None)
                .map_err(native)?;
        }
        let layouts = [owner.descriptors];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(PARAMETER_SIZE as u32)];
        let layout = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        let words = ash::util::read_spv(&mut Cursor::new(include_bytes!("shader.spv")))?;
        let shader = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: The shader has two
        // formatted storage images and checked scalar push constants; no optional
        // device feature is required by its Vulkan 1.1 SPIR-V instructions.
        unsafe {
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
        // to the same device. Local dispatch dimensions were checked above.
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
}

impl Drop for Program {
    fn drop(&mut self) {
        // SAFETY: Every accepted job retains this immutable program through its
        // bindings. Partial creation has no submitted users.
        unsafe {
            let raw = &self.device.raw;
            raw.destroy_pipeline(self.pipeline, None);
            raw.destroy_shader_module(self.shader, None);
            raw.destroy_pipeline_layout(self.layout, None);
            raw.destroy_descriptor_set_layout(self.descriptors, None);
        }
    }
}
