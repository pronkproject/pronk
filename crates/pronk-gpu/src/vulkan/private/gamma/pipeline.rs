//! Immutable gamma shader and lookup data, independent of image bindings.

use std::io::{self, Cursor};
use std::sync::Arc;

use ash::vk;

use super::table::Table;
use crate::vulkan::device::{native, unsupported, DeviceInner};

pub(super) struct Program {
    pub(super) device: Arc<DeviceInner>,
    pub(super) descriptors: vk::DescriptorSetLayout,
    pub(super) layout: vk::PipelineLayout,
    pub(super) pipeline: vk::Pipeline,
    pub(super) max_groups: [u32; 2],
    pub(super) table: Table,
    shader: vk::ShaderModule,
}

impl Program {
    pub(super) fn new(device: Arc<DeviceInner>, entries: &[[u16; 3]]) -> io::Result<Self> {
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
        if !queues[device.queue_family as usize]
            .queue_flags
            .contains(vk::QueueFlags::COMPUTE)
            || limits.max_compute_work_group_size[0] < 8
            || limits.max_compute_work_group_size[1] < 8
            || limits.max_compute_work_group_invocations < 64
        {
            return Err(unsupported("private gamma compute program is unsupported"));
        }
        let table = Table::new(Arc::clone(&device), entries)?;
        let mut owner = Self {
            device,
            descriptors: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            shader: vk::ShaderModule::null(),
            max_groups: [
                limits.max_compute_work_group_count[0],
                limits.max_compute_work_group_count[1],
            ],
            table,
        };
        let raw = &owner.device.raw;
        let bindings = [
            vk::DescriptorType::STORAGE_IMAGE,
            vk::DescriptorType::STORAGE_BUFFER,
        ]
        .into_iter()
        .enumerate()
        .map(|(binding, ty)| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding as u32)
                .descriptor_type(ty)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        })
        .collect::<Vec<_>>();
        let descriptors = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: One image and one buffer fit the mandatory device limits.
        owner.descriptors =
            unsafe { raw.create_descriptor_set_layout(&descriptors, None) }.map_err(native)?;
        let layouts = [owner.descriptors];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(12)];
        let layout = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        let words = ash::util::read_spv(&mut Cursor::new(include_bytes!("shader.spv")))?;
        let shader = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: The checked-in Vulkan 1.1 shader matches the descriptor layout
        // and 12-byte push constants and requires no optional device feature.
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
        // SAFETY: Native owners and shader interface match the checked limits.
        match unsafe { raw.create_compute_pipelines(vk::PipelineCache::null(), &create, None) } {
            Ok(pipelines) => owner.pipeline = pipelines[0],
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: Failed pipeline batches may return owned handles.
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
        // SAFETY: Accepted image jobs retain the immutable program and table.
        unsafe {
            let raw = &self.device.raw;
            raw.destroy_pipeline(self.pipeline, None);
            raw.destroy_shader_module(self.shader, None);
            raw.destroy_pipeline_layout(self.layout, None);
            raw.destroy_descriptor_set_layout(self.descriptors, None);
        }
    }
}
