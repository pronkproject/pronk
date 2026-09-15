//! Native submission and retirement for private source-reading copies.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;

use ash::vk;
use pronk_dmabuf::{export_dependencies, import_completion, Access, Completion};

use super::geometry::Transfer;
use super::PendingStage;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::{Image, SourceImage};

pub(super) fn submit(
    destination: Image,
    sources: Vec<(SourceImage, Transfer)>,
    background: Option<[u8; 3]>,
) -> io::Result<PendingStage> {
    let output = destination.export()?;
    let dst = nix::sys::stat::fstat(output.as_raw_fd())?;
    let mut allocations = HashSet::new();
    allocations.insert((dst.st_dev, dst.st_ino));
    for (source, _) in &sources {
        if source.layout().format != destination.layout().format {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging copy needs matching source and destination formats",
            ));
        }
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
        source.wait_for_producer()?;
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
            match region {
                Transfer::Copy(region) => job.device.raw.cmd_copy_image(
                    command,
                    source.raw,
                    vk::ImageLayout::GENERAL,
                    destination.raw,
                    vk::ImageLayout::GENERAL,
                    &[*region],
                ),
                Transfer::Blit(region) => job.device.raw.cmd_blit_image(
                    command,
                    source.raw,
                    vk::ImageLayout::GENERAL,
                    destination.raw,
                    vk::ImageLayout::GENERAL,
                    &[*region],
                    vk::Filter::NEAREST,
                ),
            }
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
    let completion = match completion {
        Some(sync) => sync,
        None => export_dependencies(output.as_fd(), Access::Write)?,
    };
    Ok(PendingStage::new(job, completion))
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
