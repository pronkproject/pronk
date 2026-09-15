//! Post-composition lookup on completed private pixels, before byte conversion.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::geometry::Extent;

use super::PrivateImage;
use crate::vulkan::device::unsupported;
use crate::vulkan::submission::Job;
use crate::vulkan::Device;

mod bindings;
mod pipeline;
mod table;
use bindings::Bindings;
use pipeline::Program;

pub(super) fn check_support(
    device: &crate::vulkan::device::DeviceInner,
    entries: usize,
    extent: Extent,
) -> io::Result<()> {
    pipeline::check_support(device, entries, extent)
}

/// Immutable RGB lookup data and compute program for one logical device.
///
/// Clones share the uploaded table and shader, never per-image descriptors.
/// There is no device-owned cache or reference back from the device.
#[derive(Clone)]
pub struct Gamma {
    program: Arc<Program>,
}

impl Device {
    /// Upload uniformly spaced RGB entries and prepare post-composition gamma.
    ///
    /// The table follows [`drm_display_executor::scene::color::Lut`]: 1..=65536
    /// entries, including constant and descending tables. Only LUT metadata is
    /// transferred from the CPU; raw frame pixels are never mapped or uploaded.
    /// This blocking setup should run before frame processing. Unsupported
    /// native storage limits return an error instead of changing the table.
    pub fn create_gamma(&self, entries: &[[u16; 3]]) -> io::Result<Gamma> {
        Ok(Gamma {
            program: Arc::new(Program::new(Arc::clone(&self.inner), entries)?),
        })
    }
}

struct Resources {
    bindings: Bindings,
    image: PrivateImage,
}

impl Gamma {
    /// Apply gamma in place after source acquisition and composition finish.
    ///
    /// The initialized image must belong to the program's logical device. RGB
    /// is rounded to normalized 16-bit input, sampled with integer rational LUT
    /// interpolation, then stored at normalized 16-bit precision. Alpha remains
    /// unchanged. No degamma, matrix, transfer-function inference or downstream
    /// dependency is introduced. Run on a blocking worker; errors return no
    /// image for reuse. Independent images may use clones concurrently.
    pub fn apply_waited(&self, image: PrivateImage) -> io::Result<PrivateImage> {
        if !image.initialized || !Arc::ptr_eq(&image.device, &self.program.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gamma needs an initialized private image on its program's device",
            ));
        }
        let groups = [
            image.width.get().div_ceil(8),
            image.height.get().div_ceil(8),
        ];
        if groups[0] > self.program.max_groups[0] || groups[1] > self.program.max_groups[1] {
            return Err(unsupported("private gamma dispatch is unsupported"));
        }
        let mut parameters = [0; 12];
        for (bytes, value) in parameters.chunks_exact_mut(4).zip([
            image.width.get(),
            image.height.get(),
            self.program.table.count,
        ]) {
            bytes.copy_from_slice(&value.to_ne_bytes());
        }
        let bindings = Bindings::new(Arc::clone(&self.program), &image)?;
        let mut job = Job::new(Arc::clone(&image.device), Resources { bindings, image })?;
        let barrier = job
            .resources()
            .image
            .barrier(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        // SAFETY: One invocation owns each pixel; partial workgroups are bounded
        // in the shader. The job retains the image, view, program and initialized
        // table. Descriptor ranges and push constants match the shader interface.
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
