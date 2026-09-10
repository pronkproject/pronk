//! Blending operates only on completed, non-exportable pixels.

use std::io;
use std::sync::Arc;

use ash::vk;
use drm_display_executor::scene::{
    blend::Blend,
    geometry::{Extent, SourceRect},
    transform::Transform,
};

use super::PrivateImage;
use crate::vulkan::device::unsupported;
use crate::vulkan::submission::Job;
use crate::vulkan::Device;

mod bindings;
mod geometry;
mod pipeline;
use bindings::Bindings;
use geometry::Parameters;
use pipeline::Program;

/// Reusable immutable compute program for one logical graphics device.
///
/// Clones share only compiled shader state. Every operation creates independent
/// image views and descriptors and retains them through native completion.
/// This owner retains the device; no device-owned cache retains it in return.
#[derive(Clone)]
pub struct Blender {
    program: Arc<Program>,
}

impl Device {
    /// Create a reusable private-image blend program before frame processing.
    pub fn create_blender(&self) -> io::Result<Blender> {
        Ok(Blender {
            program: Arc::new(Program::new(Arc::clone(&self.inner))?),
        })
    }
}

impl Blender {
    /// Blend a crop using the lifetime, geometry and precision contract of
    /// [`PrivateImage::blend_region_waited`], without recreating the program.
    /// Both images must belong to this program's logical device. Calls may run
    /// on separate blocking workers with independently owned images.
    pub fn blend_region_waited(
        &self,
        destination: PrivateImage,
        source: PrivateImage,
        crop: SourceRect,
        placement: [i32; 2],
        transform: Transform,
        blend: Blend,
    ) -> io::Result<BlendedImages> {
        destination.blend_with(
            Some(Arc::clone(&self.program)),
            source,
            crop,
            placement,
            transform,
            blend,
        )
    }
}

/// The unchanged source and blended private destination after GPU completion.
pub struct BlendedImages {
    pub source: PrivateImage,
    pub destination: PrivateImage,
}

struct Resources {
    // Native views are destroyed before the images they reference.
    bindings: Bindings,
    source: PrivateImage,
    destination: PrivateImage,
}

impl PrivateImage {
    /// Blend a same-sized private image over this completed destination.
    ///
    /// Both images must be initialized on one logical device. This blocking
    /// reference operation handles plane alpha and all three pixel-blend modes,
    /// with opaque output alpha. It neither imports source images nor waits for
    /// downstream reuse. Completed source acquisition precedes this operation.
    ///
    /// Color remains in its encoded RGB domain, rounded to normalized 16-bit
    /// precision after each blend. Native floating-point evaluation is not a
    /// bit-identical integer reference. No geometry, scaling, gamma or color
    /// management is applied. Errors return neither image for reuse.
    pub fn blend_waited(self, source: PrivateImage, blend: Blend) -> io::Result<BlendedImages> {
        if self.extent() != source.extent() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "whole-image blending needs equal extents",
            ));
        }
        let extent = Extent::new(source.width.get(), source.height.get())
            .expect("private extent is nonzero");
        let crop = SourceRect::new(extent, [0, 0], extent).expect("whole source crop is valid");
        self.blend_region_waited(source, crop, [0, 0], Transform::default(), blend)
    }

    /// Blend an integral source crop at an unscaled output placement.
    ///
    /// Source-axis reflection precedes counter-clockwise rotation. Clipping is
    /// resolved with checked integer geometry before dispatch; invisible crops
    /// return both images unchanged without submission. Only affected pixels
    /// receive opaque alpha. The caller should omit invisible source acquisition
    /// before reaching this private-image operation.
    ///
    /// Initialization, device, lifetime and precision requirements are those of
    /// [`Self::blend_waited`]. Fractional crops and filtering are not supported.
    pub fn blend_region_waited(
        self,
        source: PrivateImage,
        crop: SourceRect,
        placement: [i32; 2],
        transform: Transform,
        blend: Blend,
    ) -> io::Result<BlendedImages> {
        self.blend_with(None, source, crop, placement, transform, blend)
    }

    fn blend_with(
        self,
        program: Option<Arc<Program>>,
        source: PrivateImage,
        crop: SourceRect,
        placement: [i32; 2],
        transform: Transform,
        blend: Blend,
    ) -> io::Result<BlendedImages> {
        if !Arc::ptr_eq(&self.device, &source.device)
            || !self.initialized
            || !source.initialized
            || program
                .as_ref()
                .is_some_and(|program| !Arc::ptr_eq(&program.device, &self.device))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private blending needs initialized images on one device",
            ));
        }
        let source_extent = Extent::new(source.width.get(), source.height.get())
            .expect("private extent is nonzero");
        let output_extent =
            Extent::new(self.width.get(), self.height.get()).expect("private extent is nonzero");
        let Some(parameters) = Parameters::new(
            source_extent,
            output_extent,
            crop,
            placement,
            transform,
            blend,
        )?
        else {
            return Ok(BlendedImages {
                source,
                destination: self,
            });
        };
        let groups = parameters.groups;
        let program = match program {
            Some(program) => program,
            None => Arc::new(Program::new(Arc::clone(&self.device))?),
        };
        if groups[0] > program.max_groups[0] || groups[1] > program.max_groups[1] {
            return Err(unsupported("private blend dispatch is unsupported"));
        }
        let bindings = Bindings::new(program, &source, &self)?;
        let mut job = Job::new(
            Arc::clone(&self.device),
            Resources {
                bindings,
                source,
                destination: self,
            },
        )?;
        let resources = job.resources();
        let barriers = [
            resources.source.barrier(vk::AccessFlags::SHADER_READ),
            resources
                .destination
                .barrier(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE),
        ];
        let parameters = parameters.bytes();
        // SAFETY: Distinct, initialized private images outlive their views and
        // pipeline through the native job. Dispatch covers each destination
        // visible pixel once; checked clipping bounds source coordinates and
        // the shader bounds-checks partial edge workgroups. Both
        // descriptors and push constants match the checked-in shader interface.
        unsafe {
            job.device.raw.cmd_pipeline_barrier(
                job.command(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            resources.bindings.bind(job.command(), &parameters);
            job.device
                .raw
                .cmd_dispatch(job.command(), groups[0], groups[1], 1);
        }
        job.submit()?;
        let Resources {
            bindings,
            source,
            destination,
        } = job.finish()?;
        drop(bindings);
        Ok(BlendedImages {
            source,
            destination,
        })
    }
}
