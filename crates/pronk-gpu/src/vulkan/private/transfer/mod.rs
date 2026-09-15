//! Standard sRGB transfer functions over completed private pixels.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::geometry::Extent;

use super::PrivateImage;
use crate::vulkan::submission::Job;
use crate::vulkan::Device;

mod bindings;
mod pipeline;
use bindings::Bindings;
use pipeline::Program;

#[derive(Clone, Copy)]
pub(super) enum Function {
    Eotf,
    InverseEotf,
}

pub(super) fn check_support(
    device: &crate::vulkan::device::DeviceInner,
    extent: Extent,
) -> io::Result<()> {
    pipeline::check_support(device, extent)
}

#[derive(Clone)]
pub(super) struct Transfer {
    program: Arc<Program>,
}

impl Transfer {
    pub(super) fn new(device: &Device, function: Function) -> io::Result<Self> {
        Ok(Self {
            program: Arc::new(Program::new(Arc::clone(&device.inner), function)?),
        })
    }

    pub(super) fn apply_and_wait(&self, image: PrivateImage) -> io::Result<PrivateImage> {
        if !image.initialized || !Arc::ptr_eq(&image.device, &self.program.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sRGB transfer needs an initialized image on its program's device",
            ));
        }
        let groups = [
            image.width.get().div_ceil(8),
            image.height.get().div_ceil(8),
        ];
        if groups[0] > self.program.max_groups[0] || groups[1] > self.program.max_groups[1] {
            return Err(crate::vulkan::device::unsupported(
                "sRGB transfer dispatch is unsupported",
            ));
        }
        let mut parameters = [0; 12];
        for (bytes, value) in parameters.chunks_exact_mut(4).zip([
            image.width.get(),
            image.height.get(),
            u32::from(matches!(self.program.function, Function::InverseEotf)),
        ]) {
            bytes.copy_from_slice(&value.to_ne_bytes());
        }
        let bindings = Bindings::new(Arc::clone(&self.program), &image)?;
        let mut job = Job::new(Arc::clone(&image.device), Resources { bindings, image })?;
        let barrier = job
            .resources()
            .image
            .barrier(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        // SAFETY: One bounded invocation owns each pixel. Retained resources
        // and the push constants match the immutable shader interface.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            job.resources().bindings.bind(job.command(), &parameters);
            job.device
                .raw
                .cmd_dispatch(job.command(), groups[0], groups[1], 1);
        }
        job.submit()?;
        let Resources { bindings, image } = job.finish()?;
        drop(bindings);
        Ok(image)
    }
}

struct Resources {
    bindings: Bindings,
    image: PrivateImage,
}
