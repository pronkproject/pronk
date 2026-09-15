//! Whole-image initialization on the owning device, without foreign handoff.

use std::io;
use std::sync::Arc;

use ash::vk;

use super::PrivateImage;
use crate::vulkan::submission::Job;

impl PrivateImage {
    /// Initialize every private pixel with opaque RGB on a blocking worker.
    ///
    /// No external reuse wait exists for non-exportable storage. Accepted work
    /// retains the unique owner; errors do not return an image for reuse.
    pub fn clear_and_wait(self, rgb: [u8; 3]) -> io::Result<Self> {
        let mut job = Job::new(Arc::clone(&self.device), self)?;
        let image = job.resources();
        let barrier = image.barrier(vk::AccessFlags::TRANSFER_WRITE);
        let color = vk::ClearColorValue {
            float32: [
                f32::from(rgb[0]) / 255.0,
                f32::from(rgb[1]) / 255.0,
                f32::from(rgb[2]) / 255.0,
                1.0,
            ],
        };
        // SAFETY: This job uniquely owns its initialized native image and
        // transitions its tracked layout before writing the complete extent.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            job.device.raw.cmd_clear_color_image(
                job.command(),
                image.raw,
                vk::ImageLayout::GENERAL,
                &color,
                &[image.range()],
            );
        }
        job.submit()?;
        let mut image = job.finish()?;
        image.initialized = true;
        Ok(image)
    }
}
