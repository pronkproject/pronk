//! Source acquisition ends before private pixels enter output processing.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::geometry::{Extent, SourceRect};
use pronk_dmabuf::{export_dependencies, import_completion, Access};

use super::PrivateImage;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::SourceImage;

mod geometry;
mod pending;
pub use pending::PendingPrivateRead;

use geometry::Blit;

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
    /// This blocking API does not expose an early source-accounting record.
    pub fn copy_into_private_and_wait(self, destination: PrivateImage) -> io::Result<PrivateImage> {
        self.submit_private_copy(destination)?.wait()
    }

    /// Submit a source read into non-exportable private storage.
    ///
    /// Validation and producer waits follow [`Self::copy_into_private_and_wait`].
    /// Return exposes a native completion record without waiting for the copy.
    /// The pending owner retains both images until native retirement; it must
    /// remain on a blocking graphics worker because drop may wait for GPU work.
    pub fn submit_private_copy(self, destination: PrivateImage) -> io::Result<PendingPrivateRead> {
        if (self.layout().width, self.layout().height) != destination.extent() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private source copy needs matching image dimensions",
            ));
        }
        let extent = image_extent(self.layout().width.get(), self.layout().height.get())?;
        let crop = SourceRect::new(extent, [0, 0], extent).map_err(invalid)?;
        self.submit_private_region(destination, crop, [0, 0], extent, [0; 3])
    }

    /// Read a cropped source into a bounded region of private storage.
    ///
    /// The region is scaled using Vulkan's nearest-texel blit filter. Sampling
    /// uses pixel centers; rounding at texel boundaries is implementation-defined.
    /// Every pixel outside the region is initialized to the opaque background,
    /// including when the destination contains an older completed frame.
    /// Source alpha is copied, without blending or color-space conversion.
    ///
    /// Ownership, producer waits, submission accounting and blocking destruction
    /// follow [`Self::submit_private_copy`]. No exported destination is involved.
    pub fn submit_private_region(
        self,
        destination: PrivateImage,
        crop: SourceRect,
        position: [u32; 2],
        extent: Extent,
        background: [u8; 3],
    ) -> io::Result<PendingPrivateRead> {
        if !Arc::ptr_eq(&self.external.device, &destination.device) {
            return Err(invalid("private source read needs images on one device"));
        }
        let blit = Blit::new(
            image_extent(self.layout().width.get(), self.layout().height.get())?,
            image_extent(destination.width.get(), destination.height.get())?,
            crop,
            position,
            extent,
        )?;
        self.wait_for_producer()?;
        require_success(
            export_dependencies(self.external.fd.as_fd(), Access::Read)?.wait_blocking()?,
        )?;
        let mut job = Job::new(Arc::clone(&self.external.device), (self, destination))?;
        let (source, destination) = job.resources();
        let range = destination.range();
        let source_barrier = vk::ImageMemoryBarrier::default()
            .image(source.external.raw)
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
        // SAFETY: The source import contract supplies GENERAL foreign release
        // and producer completion. Queried formats support blits, and both
        // rectangles have checked bounds and signed edges. The unique private
        // allocation cannot alias the source. The job owns both images and
        // orders background initialization before the region overwrites it.
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
            if !blit.fills_destination {
                let color = vk::ClearColorValue {
                    float32: [
                        f32::from(background[0]) / 255.0,
                        f32::from(background[1]) / 255.0,
                        f32::from(background[2]) / 255.0,
                        1.0,
                    ],
                };
                job.device.raw.cmd_clear_color_image(
                    job.command(),
                    destination.raw,
                    vk::ImageLayout::GENERAL,
                    &color,
                    &[range],
                );
                let cleared = vk::ImageMemoryBarrier::default()
                    .image(destination.raw)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .subresource_range(range);
                job.device.raw.cmd_pipeline_barrier(
                    job.command(),
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[cleared],
                );
            }
            job.device.raw.cmd_blit_image(
                job.command(),
                source.external.raw,
                vk::ImageLayout::GENERAL,
                destination.raw,
                vk::ImageLayout::GENERAL,
                &[blit.region],
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
            import_completion(job.resources().0.external.fd.as_fd(), Access::Read, sync)?;
        }
        Ok(PendingPrivateRead::new(job, completion))
    }
}

fn image_extent(width: u32, height: u32) -> io::Result<Extent> {
    Extent::new(width, height).map_err(invalid)
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}
