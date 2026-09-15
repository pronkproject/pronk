//! Waited copies between executor-owned images, independent of source grants.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, SyncFile};

use super::image::ImageState;
use super::submission::{require_success, Job};
use super::Image;

/// Both images return only after the copy ends successfully.
pub struct CopiedImages {
    pub source: Image,
    pub destination: Image,
    pub completion: SyncFile,
}

#[cfg(test)]
mod tests;

impl Image {
    /// Copy a complete initialized image into this same-sized destination.
    ///
    /// Both allocations must belong to the same Vulkan device. The caller owns
    /// exclusive access across dependency snapshot and completion enrollment.
    /// This operation runs on a blocking graphics worker, never a media loop.
    ///
    /// Use executor-owned staging as the source: destination reuse may wait,
    /// so no compositor-source lease belongs in this operation. Accepted work
    /// retains both images; errors do not return either image for reuse.
    pub fn copy_from_and_wait(self, source: Image) -> io::Result<CopiedImages> {
        if !Arc::ptr_eq(&self.device, &source.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy images belong to different Vulkan devices",
            ));
        }
        let layout = self.layout();
        let input = source.layout();
        if layout.width != input.width
            || layout.height != input.height
            || layout.format != input.format
            || source.state != ImageState::Released
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy needs an initialized source with matching dimensions and format",
            ));
        }
        let destination_fd = self.export()?;
        let source_fd = source.export()?;
        require_success(
            export_dependencies(destination_fd.as_fd(), Access::Write)?.wait_blocking()?,
        )?;
        require_success(export_dependencies(source_fd.as_fd(), Access::Read)?.wait_blocking()?)?;
        let mut job = Job::new(Arc::clone(&self.device), (source, self))?;
        let command = job.command();
        let (source, destination) = job.resources();
        let acquire = [
            source.acquire_barrier(vk::AccessFlags::TRANSFER_READ),
            destination.acquire_barrier(vk::AccessFlags::TRANSFER_WRITE),
        ];
        let release = [
            source.release_barrier(vk::AccessFlags::TRANSFER_READ),
            destination.release_barrier(vk::AccessFlags::TRANSFER_WRITE),
        ];
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let region = vk::ImageCopy::default()
            .src_subresource(layers)
            .dst_subresource(layers)
            .extent(vk::Extent3D {
                width: layout.width.get(),
                height: layout.height.get(),
                depth: 1,
            });
        // SAFETY: Distinct owned allocations on one device have matching formats
        // and extents. The initialized source and destination have completed their
        // native dependencies. Job ownership retains both through submission.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &acquire,
            );
            job.device.raw.cmd_copy_image(
                command,
                source.raw,
                vk::ImageLayout::GENERAL,
                destination.raw,
                vk::ImageLayout::GENERAL,
                &[region],
            );
            job.device.raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &release,
            );
        }
        job.submit()?;
        let completion = job.export_completion()?;
        if let Some(sync) = &completion {
            import_completion(source_fd.as_fd(), Access::Read, sync)?;
            import_completion(destination_fd.as_fd(), Access::Write, sync)?;
        }
        let (source, mut destination) = job.finish()?;
        let completion = match completion {
            Some(sync) => sync,
            None => export_dependencies(destination_fd.as_fd(), Access::Write)?,
        };
        require_success(
            SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        destination.state = ImageState::Released;
        Ok(CopiedImages {
            source,
            destination,
            completion,
        })
    }
}
