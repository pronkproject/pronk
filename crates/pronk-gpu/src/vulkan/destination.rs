//! Write-only imports for independently authorized recipient storage.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, SyncFile};

use super::external::ExternalImage;
use super::image::{ImageState, ImageUse};
use super::submission::{require_success, Job};
use super::{Device, Image, ImageLayout};

/// One externally owned image imported only as a native write destination.
///
/// The type exposes no source, allocation, export, or reuse operation. Its
/// caller retains the independently issued recipient claim that authorizes the
/// eventual write and reports native completion.
///
/// ```compile_fail
/// use pronk_gpu::vulkan::DestinationImage;
/// fn export_recipient(image: DestinationImage) {
///     image.export();
/// }
/// ```
pub struct DestinationImage {
    external: ExternalImage,
}

/// A completed private-to-recipient copy and its native completion record.
pub struct DestinationCopy {
    pub source: Image,
    pub destination: DestinationImage,
    pub completion: SyncFile,
}

impl Device {
    /// Import an exact single-plane DMA-BUF layout for recipient writes.
    ///
    /// Import creates native objects but performs no access and waits for no
    /// dependency. The eventual copy snapshots the destination's write
    /// dependencies before submission.
    ///
    /// # Safety
    ///
    /// The descriptor and metadata must identify a compatible image allocation
    /// on this physical GPU. The caller must hold exclusive recipient-write
    /// authority, prevent new external access until terminal release, and use
    /// the returned image only with the output job that issued that authority.
    pub unsafe fn import_destination(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
    ) -> io::Result<DestinationImage> {
        // SAFETY: The caller supplies the destination-specific cross-API
        // contract documented above.
        let external = unsafe {
            self.import_external_image(fd, layout, ImageUse::ImportedDestination, Access::Write)
        }?;
        Ok(DestinationImage { external })
    }
}

impl DestinationImage {
    pub fn layout(&self) -> ImageLayout {
        self.external.layout
    }

    pub fn is_owned_by(&self, device: &Device) -> bool {
        Arc::ptr_eq(&self.external.device, &device.inner)
    }

    /// Copy one completed renderer-owned image into this recipient allocation.
    ///
    /// The operation may wait for recipient reuse, but its source is independent
    /// renderer storage rather than a compositor framebuffer. Both owners remain
    /// retained through native completion, and errors return neither for reuse.
    pub fn copy_from_and_wait(self, source: Image) -> io::Result<DestinationCopy> {
        if !Arc::ptr_eq(&self.external.device, &source.device) {
            return Err(invalid("recipient copy needs images on one device"));
        }
        let input = source.layout();
        let output = self.layout();
        if input.width != output.width
            || input.height != output.height
            || input.format != output.format
            || source.state != ImageState::Released
        {
            return Err(invalid(
                "recipient copy needs an initialized matching source",
            ));
        }
        let source_fd = source.export()?;
        let source_stat = nix::sys::stat::fstat(source_fd.as_fd().as_raw_fd())?;
        let destination_stat = nix::sys::stat::fstat(self.external.fd.as_fd().as_raw_fd())?;
        if (source_stat.st_dev, source_stat.st_ino)
            == (destination_stat.st_dev, destination_stat.st_ino)
        {
            return Err(invalid("recipient destination aliases its private source"));
        }
        require_success(export_dependencies(source_fd.as_fd(), Access::Read)?.wait_blocking()?)?;
        require_success(
            export_dependencies(self.external.fd.as_fd(), Access::Write)?.wait_blocking()?,
        )?;

        let mut job = Job::new(Arc::clone(&source.device), (source, self))?;
        let (source, destination) = job.resources();
        let range = source.color_range();
        let acquire = [
            source.acquire_barrier(vk::AccessFlags::TRANSFER_READ),
            vk::ImageMemoryBarrier::default()
                .image(destination.external.raw)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(job.device.queue_family)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(range),
        ];
        let release = [
            source.release_barrier(vk::AccessFlags::TRANSFER_READ),
            vk::ImageMemoryBarrier::default()
                .image(destination.external.raw)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(job.device.queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(range),
        ];
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let region = vk::ImageCopy::default()
            .src_subresource(layers)
            .dst_subresource(layers)
            .extent(vk::Extent3D {
                width: input.width.get(),
                height: input.height.get(),
                depth: 1,
            });
        // SAFETY: The checked images are distinct, matching single-plane
        // allocations on one device. Reservation waits precede submission, and
        // the job retains both resources through native completion.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &acquire,
            );
            job.device.raw.cmd_copy_image(
                job.command(),
                source.raw,
                vk::ImageLayout::GENERAL,
                destination.external.raw,
                vk::ImageLayout::GENERAL,
                &[region],
            );
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::TRANSFER,
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
            import_completion(job.resources().1.external.fd.as_fd(), Access::Write, sync)?;
        }
        let (source, destination) = job.finish()?;
        let completion = match completion {
            Some(sync) => sync,
            None => export_dependencies(destination.external.fd.as_fd(), Access::Write)?,
        };
        require_success(
            SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        Ok(DestinationCopy {
            source,
            destination,
            completion,
        })
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests;
