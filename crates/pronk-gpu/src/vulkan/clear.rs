//! Synchronous generated-image production for dedicated blocking graphics workers.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, SyncFile};

use super::image::ImageState;
use super::submission::{require_success, Job};
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
    pub fn clear_and_wait(self, rgb: [u8; 3]) -> io::Result<(Self, SyncFile)> {
        self.clear_rgba_and_wait([rgb[0], rgb[1], rgb[2], 255])
    }

    /// Fill the image with explicit RGBA channel values, without premultiplying.
    ///
    /// Exclusive native access, dependency waits and blocking-worker requirements
    /// are identical to [`Self::clear_and_wait`]. Channels are quantized to the
    /// selected format, and alpha is ignored when that format has no alpha.
    /// Successful completion does not establish the blending or media policy.
    pub fn clear_rgba_and_wait(self, rgba: [u8; 4]) -> io::Result<(Self, SyncFile)> {
        self.clear_rgba16_and_wait(rgba.map(|channel| u16::from(channel) * 257))
    }

    /// Fill the image with normalized 16-bit RGBA channel values.
    ///
    /// Channels span zero through 65535 and are quantized to the destination's
    /// packed format using Vulkan's normalized fixed-point conversion rules,
    /// which permit either adjacent representable value. Higher-depth sources
    /// need no CPU pixel writes. It does not change color space, premultiply
    /// alpha or imply HDR. Ownership and native waits follow
    /// [`Self::clear_rgba_and_wait`].
    pub fn clear_rgba16_and_wait(self, rgba: [u16; 4]) -> io::Result<(Self, SyncFile)> {
        let buffer = self.export()?;
        require_success(export_dependencies(buffer.as_fd(), Access::Write)?.wait_blocking()?)?;
        let mut job = Job::new(Arc::clone(&self.device), self)?;
        let command = job.command();
        let image = job.resources();
        let range = image.color_range();
        let acquire = image.acquire_barrier(vk::AccessFlags::TRANSFER_WRITE);
        let release = image.release_barrier(vk::AccessFlags::TRANSFER_WRITE);
        let color = vk::ClearColorValue {
            float32: rgba.map(|channel| f32::from(channel) / 65535.0),
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
        image.state = ImageState::Released;
        Ok((image, completion))
    }
}

#[cfg(test)]
mod tests;
