//! Conversion from completed private pixels into separately owned shared output.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, SyncFile};

use super::PrivateImage;
use crate::vulkan::image::ImageState;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::Image;

/// Private storage and initialized shared output after successful conversion.
pub struct PrivateCopy {
    pub source: PrivateImage,
    pub destination: Image,
    pub completion: SyncFile,
}

impl PrivateImage {
    /// Convert completed floating-point RGBA into a same-sized shared image.
    ///
    /// This blocking output operation may wait for downstream destination reuse;
    /// no compositor-source claims belong here. The private owner exposes no
    /// imports or deferred source readers. The caller must exclude competing
    /// destination accesses throughout dependency snapshot and submission.
    ///
    /// Conversion uses a nearest-neighbor Vulkan format blit, with no scaling or
    /// color-space conversion. Errors return neither allocation for reuse.
    pub fn copy_into_waited(self, destination: Image) -> io::Result<PrivateCopy> {
        if !Arc::ptr_eq(&self.device, &destination.device)
            || self.extent() != (destination.layout().width, destination.layout().height)
            || !self.initialized
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private output copy needs initialized matching images on one device",
            ));
        }
        let output = destination.export()?;
        require_success(export_dependencies(output.as_fd(), Access::Write)?.wait_blocking()?)?;
        let mut job = Job::new(Arc::clone(&self.device), (self, destination))?;
        let (source, destination) = job.resources();
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let offsets = [
            vk::Offset3D::default(),
            vk::Offset3D {
                x: source.width.get() as i32,
                y: source.height.get() as i32,
                z: 1,
            },
        ];
        let blit = vk::ImageBlit::default()
            .src_subresource(layers)
            .src_offsets(offsets)
            .dst_subresource(layers)
            .dst_offsets(offsets);
        let acquire = [
            source.barrier(vk::AccessFlags::TRANSFER_READ),
            destination.acquire_barrier(vk::AccessFlags::TRANSFER_WRITE),
        ];
        let release = destination.release_barrier(vk::AccessFlags::TRANSFER_WRITE);
        // SAFETY: Both queried formats support blits. Distinct owned images
        // have matching checked signed extents and completed prior accesses.
        // The job retains both images until native completion.
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
            import_completion(output.as_fd(), Access::Write, sync)?;
        }
        let (source, mut destination) = job.finish()?;
        let completion = match completion {
            Some(sync) => sync,
            None => export_dependencies(output.as_fd(), Access::Write)?,
        };
        require_success(
            SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        destination.state = ImageState::Released;
        Ok(PrivateCopy {
            source,
            destination,
            completion,
        })
    }
}
