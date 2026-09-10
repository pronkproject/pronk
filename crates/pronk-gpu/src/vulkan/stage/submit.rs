//! Native submission and retirement for private source-reading copies.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, Completion, SyncFile};

use crate::vulkan::image::ImageState;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::{Image, SourceImage};

pub(super) fn copy_waited(
    destination: Image,
    sources: Vec<(SourceImage, vk::ImageCopy)>,
    background: Option<[u8; 3]>,
) -> io::Result<(Image, SyncFile)> {
    let output = destination.export()?;
    let dst = nix::sys::stat::fstat(output.as_raw_fd())?;
    let mut allocations = HashSet::new();
    allocations.insert((dst.st_dev, dst.st_ino));
    for (source, _) in &sources {
        if !Arc::ptr_eq(&source.device, &destination.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging copy needs one Vulkan device",
            ));
        }
        let src = nix::sys::stat::fstat(source.fd.as_raw_fd())?;
        if !allocations.insert((src.st_dev, src.st_ino)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging inputs and destination must be distinct allocations",
            ));
        }
    }
    require_available(export_dependencies(output.as_fd(), Access::Write)?.completion()?)?;
    for (source, _) in &sources {
        require_success(
            SyncFile::from_fd(source.producer.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        require_success(export_dependencies(source.fd.as_fd(), Access::Read)?.wait_blocking()?)?;
    }
    let mut job = Job::new(Arc::clone(&destination.device), (sources, destination))?;
    let (sources, destination) = job.resources();
    let command = job.command();
    let range = destination.color_range();
    let mut acquire = vec![destination.acquire_barrier(vk::AccessFlags::TRANSFER_WRITE)];
    let mut release = vec![destination.release_barrier(vk::AccessFlags::TRANSFER_WRITE)];
    for (source, _) in sources {
        let source_acquire = vk::ImageMemoryBarrier::default()
            .image(source.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(job.device.queue_family)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(range);
        let source_release = vk::ImageMemoryBarrier::default()
            .image(source.raw)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(job.device.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(range);
        acquire.push(source_acquire);
        release.push(source_release);
    }
    // SAFETY: Valid imported source regions, distinct owned allocations and
    // completed native dependencies. The job retains every allocation; the
    // caller excludes external reuse throughout reading and foreign release.
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
        if let Some(rgb) = background {
            let color = vk::ClearColorValue {
                float32: [
                    f32::from(rgb[0]) / 255.0,
                    f32::from(rgb[1]) / 255.0,
                    f32::from(rgb[2]) / 255.0,
                    1.0,
                ],
            };
            job.device.raw.cmd_clear_color_image(
                command,
                destination.raw,
                vk::ImageLayout::GENERAL,
                &color,
                &[range],
            );
        }
        for (index, (source, region)) in sources.iter().enumerate() {
            if background.is_some() || index != 0 {
                // Order background and lower-plane writes before this plane.
                let preceding_write = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                job.device.raw.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[preceding_write],
                    &[],
                    &[],
                );
            }
            job.device.raw.cmd_copy_image(
                command,
                source.raw,
                vk::ImageLayout::GENERAL,
                destination.raw,
                vk::ImageLayout::GENERAL,
                &[*region],
            );
        }
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
        for (source, _) in &job.resources().0 {
            import_completion(source.fd.as_fd(), Access::Read, sync)?;
        }
        import_completion(output.as_fd(), Access::Write, sync)?;
    }
    let (sources, mut destination) = job.finish()?;
    drop(sources);
    let completion = match completion {
        Some(sync) => sync,
        None => export_dependencies(output.as_fd(), Access::Write)?,
    };
    require_success(SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?.wait_blocking()?)?;
    destination.state = ImageState::Released;
    Ok((destination, completion))
}

pub(super) fn require_available(completion: Option<Completion>) -> io::Result<()> {
    match completion {
        Some(completion) => require_success(completion),
        None => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "private staging destination still has native users",
        )),
    }
}
