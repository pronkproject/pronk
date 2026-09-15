//! Exact post-composition color matrix over completed private pixels.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::color::ColorMatrix;

use super::PrivateImage;
use crate::vulkan::device::unsupported;
use crate::vulkan::submission::Job;
use crate::vulkan::Device;

mod bindings;
mod pipeline;
use bindings::Bindings;
use pipeline::Program;

/// Immutable signed color matrix program for one logical device.
#[derive(Clone)]
pub struct OutputMatrix {
    program: Arc<Program>,
}

impl Device {
    /// Prepare an exact DRM S31.32 output matrix.
    pub fn create_output_matrix(&self, matrix: ColorMatrix) -> io::Result<OutputMatrix> {
        Ok(OutputMatrix {
            program: Arc::new(Program::new(Arc::clone(&self.inner), matrix)?),
        })
    }
}

struct Resources {
    bindings: Bindings,
    image: PrivateImage,
}

impl OutputMatrix {
    /// Apply the matrix after composition and before a gamma lookup table.
    ///
    /// The initialized image must belong to the program's logical device.
    /// Signed products use a two-word accumulator, then round and saturate with
    /// the CPU reference's rules. Alpha remains unchanged. Errors return no
    /// image for reuse.
    pub fn apply_waited(&self, image: PrivateImage) -> io::Result<PrivateImage> {
        if !image.initialized || !Arc::ptr_eq(&image.device, &self.program.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output matrix needs an initialized private image on its program's device",
            ));
        }
        let groups = [
            image.width.get().div_ceil(8),
            image.height.get().div_ceil(8),
        ];
        if groups[0] > self.program.max_groups[0] || groups[1] > self.program.max_groups[1] {
            return Err(unsupported("output matrix dispatch is unsupported"));
        }
        let mut parameters = [0; pipeline::PARAMETER_SIZE];
        for (bytes, value) in parameters[..8]
            .chunks_exact_mut(4)
            .zip([image.width.get(), image.height.get()])
        {
            bytes.copy_from_slice(&value.to_ne_bytes());
        }
        for (bytes, value) in parameters[8..]
            .chunks_exact_mut(8)
            .zip(self.program.matrix.sign_magnitude())
        {
            bytes.copy_from_slice(&value.to_ne_bytes());
        }
        let bindings = Bindings::new(Arc::clone(&self.program), &image)?;
        let mut job = Job::new(Arc::clone(&image.device), Resources { bindings, image })?;
        let barrier = job
            .resources()
            .image
            .barrier(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        // SAFETY: One bounded invocation owns each pixel. The retained image,
        // bindings, pipeline and push constants match the shader interface.
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
