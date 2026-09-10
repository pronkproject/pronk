//! Source acquisition ends before private pixels enter output processing.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, SyncFile};

use super::PrivateImage;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::SourceImage;

mod pending;
pub use pending::PendingPrivateRead;

impl SourceImage {
    /// Read the entire source into same-sized, non-exportable private storage.
    ///
    /// The blocking operation waits only for source producers and its own GPU
    /// work. Private storage has no external users or destination reuse fence.
    /// Vulkan converts packed UNORM channels to floating point, preserving
    /// encoded RGB and pixel alpha without blending or color-space conversion.
    ///
    /// The caller retains source authority and excludes pixel reuse throughout
    /// the call. Successful return destroys the import after reading completes;
    /// no source lease belongs to subsequent private-image or output work.
    /// Errors return neither image for reuse. Run on a blocking graphics worker.
    /// This waited API does not expose an early source-accounting record.
    pub fn copy_into_private_waited(self, destination: PrivateImage) -> io::Result<PrivateImage> {
        self.submit_private_copy(destination)?.wait()
    }

    /// Submit a source read into non-exportable private storage.
    ///
    /// Validation and producer waits follow [`Self::copy_into_private_waited`].
    /// Return exposes a native completion record without waiting for the copy.
    /// The pending owner retains both images until native retirement; it must
    /// remain on a blocking graphics worker because drop may wait for GPU work.
    pub fn submit_private_copy(self, destination: PrivateImage) -> io::Result<PendingPrivateRead> {
        if !Arc::ptr_eq(&self.device, &destination.device)
            || (self.layout().width, self.layout().height) != destination.extent()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private source copy needs matching images on one device",
            ));
        }
        require_success(
            SyncFile::from_fd(self.producer.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        require_success(export_dependencies(self.fd.as_fd(), Access::Read)?.wait_blocking()?)?;
        let mut job = Job::new(Arc::clone(&self.device), (self, destination))?;
        let (source, destination) = job.resources();
        let range = destination.range();
        let source_barrier = vk::ImageMemoryBarrier::default()
            .image(source.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .subresource_range(range);
        let acquire = [
            source_barrier
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(job.device.queue_family)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
            destination.barrier(vk::AccessFlags::TRANSFER_WRITE),
        ];
        let release = source_barrier
            .src_queue_family_index(job.device.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ);
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let offsets = [
            vk::Offset3D::default(),
            vk::Offset3D {
                x: destination.width.get() as i32,
                y: destination.height.get() as i32,
                z: 1,
            },
        ];
        let blit = vk::ImageBlit::default()
            .src_subresource(layers)
            .src_offsets(offsets)
            .dst_subresource(layers)
            .dst_offsets(offsets);
        // SAFETY: The source import contract supplies GENERAL foreign release
        // and producer completion. Queried formats support blits, and private
        // allocation checked signed extents. The unique private allocation is
        // never exported, so cannot alias the source. The job owns both images.
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
            job.device.raw.cmd_blit_image(
                job.command(),
                source.raw,
                vk::ImageLayout::GENERAL,
                destination.raw,
                vk::ImageLayout::GENERAL,
                &[blit],
                vk::Filter::NEAREST,
            );
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::TRANSFER,
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
            import_completion(job.resources().0.fd.as_fd(), Access::Read, sync)?;
        }
        Ok(PendingPrivateRead::new(job, completion))
    }
}
