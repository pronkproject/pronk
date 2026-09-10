//! Whole-image blending operates only on completed, non-exportable pixels.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::blend::{Blend, PixelBlend};

use super::PrivateImage;
use crate::vulkan::device::unsupported;
use crate::vulkan::submission::Job;

mod pipeline;
use pipeline::Pipeline;

/// The unchanged source and blended private destination after GPU completion.
pub struct BlendedImages {
    pub source: PrivateImage,
    pub destination: PrivateImage,
}

struct Resources {
    // Native views are destroyed before the images they reference.
    pipeline: Pipeline,
    source: PrivateImage,
    destination: PrivateImage,
}

impl PrivateImage {
    /// Blend a same-sized private image over this completed destination.
    ///
    /// Both images must be initialized on one logical device. This blocking
    /// reference operation handles plane alpha and all three pixel-blend modes,
    /// with opaque output alpha. It neither imports source images nor waits for
    /// downstream reuse. Completed source acquisition precedes this operation.
    ///
    /// Color remains in its encoded RGB domain, rounded to normalized 16-bit
    /// precision after each blend. Native floating-point evaluation is not a
    /// bit-identical integer reference. No geometry, scaling, gamma or color
    /// management is applied. Errors return neither image for reuse.
    pub fn blend_waited(self, source: PrivateImage, blend: Blend) -> io::Result<BlendedImages> {
        if !Arc::ptr_eq(&self.device, &source.device)
            || self.extent() != source.extent()
            || !self.initialized
            || !source.initialized
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private blending needs initialized matching images on one device",
            ));
        }
        let groups = [self.width.get().div_ceil(8), self.height.get().div_ceil(8)];
        // SAFETY: The image retains the physical device and its instance.
        let queues = unsafe {
            self.device
                .instance()
                .get_physical_device_queue_family_properties(self.device.physical)
        };
        let limits = unsafe {
            self.device
                .instance()
                .get_physical_device_properties(self.device.physical)
        }
        .limits;
        if !queues[self.device.queue_family as usize]
            .queue_flags
            .contains(vk::QueueFlags::COMPUTE)
            || limits.max_compute_work_group_size[0] < 8
            || limits.max_compute_work_group_size[1] < 8
            || limits.max_compute_work_group_invocations < 64
            || groups[0] > limits.max_compute_work_group_count[0]
            || groups[1] > limits.max_compute_work_group_count[1]
        {
            return Err(unsupported("private blend dispatch is unsupported"));
        }
        let pipeline = Pipeline::new(&source, &self)?;
        let mut job = Job::new(
            Arc::clone(&self.device),
            Resources {
                pipeline,
                source,
                destination: self,
            },
        )?;
        let resources = job.resources();
        let barriers = [
            resources.source.barrier(vk::AccessFlags::SHADER_READ),
            resources
                .destination
                .barrier(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE),
        ];
        let mode: u32 = match blend.pixel {
            PixelBlend::None => 0,
            PixelBlend::Premultiplied => 1,
            PixelBlend::Coverage => 2,
        };
        let mut parameters = [0u8; 8];
        parameters[..4].copy_from_slice(&mode.to_ne_bytes());
        parameters[4..].copy_from_slice(&u32::from(blend.plane_alpha).to_ne_bytes());
        // SAFETY: Distinct, initialized private images outlive their views and
        // pipeline through the native job. Dispatch covers each destination
        // pixel once; the shader bounds-checks partial edge workgroups. Both
        // descriptors and push constants match the checked-in shader interface.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            resources
                .pipeline
                .bind(&job.device.raw, job.command(), &parameters);
            job.device
                .raw
                .cmd_dispatch(job.command(), groups[0], groups[1], 1);
        }
        job.submit()?;
        let Resources {
            pipeline,
            source,
            destination,
        } = job.finish()?;
        drop(pipeline);
        Ok(BlendedImages {
            source,
            destination,
        })
    }
}
