//! Waited generated-image production for dedicated blocking graphics workers.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, Completion, SyncFile};

use super::submission::Job;
use super::Image;

impl Image {
    /// Fill the whole image with opaque RGB pixels on a blocking graphics worker.
    ///
    /// The caller must have exclusive native access, including transport return,
    /// and keep compositor-source leases out of this output-only operation. The
    /// image is consumed until submitted work ends. Errors do not return an image
    /// for reuse. This method must not run on a PipeWire loop or Tokio worker.
    ///
    /// The returned native completion is ready for the output-pool handoff. No
    /// CPU pixel mapping is performed. Use `spawn_blocking` from async callers.
    pub fn clear_waited(self, rgb: [u8; 3]) -> io::Result<(Self, SyncFile)> {
        let buffer = self.export()?;
        require_success(export_dependencies(buffer.as_fd(), Access::Write)?.wait_blocking()?)?;
        let mut job = Job::new(Arc::clone(&self.device), self)?;
        let command = job.command();
        let image = job.resources();
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let acquire = vk::ImageMemoryBarrier::default()
            .image(image.raw)
            .old_layout(if image.external {
                vk::ImageLayout::GENERAL
            } else {
                vk::ImageLayout::UNDEFINED
            })
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(if image.external {
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .dst_queue_family_index(if image.external {
                job.device.queue_family
            } else {
                vk::QUEUE_FAMILY_IGNORED
            })
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .subresource_range(range);
        let release = vk::ImageMemoryBarrier::default()
            .image(image.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(job.device.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .subresource_range(range);
        let color = vk::ClearColorValue {
            float32: [
                f32::from(rgb[0]) / 255.0,
                f32::from(rgb[1]) / 255.0,
                f32::from(rgb[2]) / 255.0,
                1.0,
            ],
        };
        // SAFETY: The recording command buffer and image belong to this device.
        // Exclusive caller ownership and the completed reservation snapshot permit
        // acquisition; the entire image is initialized before foreign release.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            job.device.raw.cmd_clear_color_image(
                command,
                image.raw,
                vk::ImageLayout::GENERAL,
                &color,
                &[range],
            );
            job.device.raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
        }
        job.submit()?;
        let completion = job.export_completion()?;
        if let Some(sync) = &completion {
            import_completion(buffer.as_fd(), Access::Write, sync)?;
        }
        let mut image = job.finish()?;
        // Preserve a materialized descriptor for the pool even when Vulkan used
        // the completed sentinel. No future GPU work is represented by that case.
        let completion = match completion {
            Some(sync) => sync,
            None => export_dependencies(buffer.as_fd(), Access::Write)?,
        };
        let check = SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?;
        require_success(check.wait_blocking()?)?;
        image.external = true;
        Ok((image, completion))
    }
}

fn require_success(completion: Completion) -> io::Result<()> {
    match completion {
        Completion::Success => Ok(()),
        Completion::Failed(error) => Err(io::Error::other(format!(
            "native GPU dependency failed: {error}"
        ))),
    }
}
