use std::io::{self, Cursor};
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::color::ColorMatrix;

use crate::vulkan::device::{native, unsupported, DeviceInner};

pub(super) const PARAMETER_SIZE: usize = 8 + 12 * 8;

pub(super) struct Program {
    pub(super) device: Arc<DeviceInner>,
    pub(super) descriptors: vk::DescriptorSetLayout,
    pub(super) layout: vk::PipelineLayout,
    pub(super) pipeline: vk::Pipeline,
    pub(super) max_groups: [u32; 2],
    pub(super) matrix: ColorMatrix,
    shader: vk::ShaderModule,
}

impl Program {
    pub(super) fn new(device: Arc<DeviceInner>, matrix: ColorMatrix) -> io::Result<Self> {
        // SAFETY: The retained owner keeps the physical device and instance live.
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
        if !device.shader_int64
            || !queues[device.queue_family as usize]
                .queue_flags
                .contains(vk::QueueFlags::COMPUTE)
            || limits.max_compute_work_group_size[0] < 8
            || limits.max_compute_work_group_size[1] < 8
            || limits.max_compute_work_group_invocations < 64
            || limits.max_push_constants_size < PARAMETER_SIZE as u32
        {
            return Err(unsupported("exact output matrices are unsupported"));
        }
        let mut owner = Self {
            device,
            descriptors: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            max_groups: [
                limits.max_compute_work_group_count[0],
                limits.max_compute_work_group_count[1],
            ],
            matrix,
            shader: vk::ShaderModule::null(),
        };
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE)];
        let descriptors = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: The single storage image fits the mandatory descriptor limit.
        owner.descriptors = unsafe {
            owner
                .device
                .raw
                .create_descriptor_set_layout(&descriptors, None)
        }
        .map_err(native)?;
        let layouts = [owner.descriptors];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(PARAMETER_SIZE as u32)];
        let layout = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        let words = ash::util::read_spv(&mut Cursor::new(include_bytes!("shader.spv")))?;
        let shader = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: The checked-in shader matches the descriptor layout and
        // 104-byte push-constant range. Its required feature was enabled.
        unsafe {
            owner.layout = owner
                .device
                .raw
                .create_pipeline_layout(&layout, None)
                .map_err(native)?;
            owner.shader = owner
                .device
                .raw
                .create_shader_module(&shader, None)
                .map_err(native)?;
        }
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(owner.shader)
            .name(c"main");
        let create = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(owner.layout)];
        // SAFETY: Native owners and shader interface match the checked limits.
        match unsafe {
            owner
                .device
                .raw
                .create_compute_pipelines(vk::PipelineCache::null(), &create, None)
        } {
            Ok(pipelines) => owner.pipeline = pipelines[0],
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: Failed pipeline batches may return owned handles.
                    unsafe { owner.device.raw.destroy_pipeline(pipeline, None) };
                }
                return Err(native(error));
            }
        }
        Ok(owner)
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        // SAFETY: Accepted image jobs retain the immutable program.
        unsafe {
            let raw = &self.device.raw;
            raw.destroy_pipeline(self.pipeline, None);
            raw.destroy_shader_module(self.shader, None);
            raw.destroy_pipeline_layout(self.layout, None);
            raw.destroy_descriptor_set_layout(self.descriptors, None);
        }
    }
}
