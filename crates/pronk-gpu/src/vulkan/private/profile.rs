//! Whole-scene requirements checked without importing or allocating images.

use std::io;
use std::num::NonZeroU32;

use drm_display_executor::scene::{
    blend::Blend,
    color::{ColorPipeline, OutputColor},
    geometry::{DestinationRect, Extent, SourceRect},
    transform::Transform,
};

use super::Blender;
use crate::vulkan::{Device, PackedFormat};

/// The exact external storage needed by one scene layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRequirements {
    pub format: PackedFormat,
    pub extent: Extent,
    pub modifier: u64,
}

/// Source storage and the operation needed to place one layer.
#[derive(Clone, Copy, Debug)]
pub struct LayerRequirements<'a> {
    pub source: SourceRequirements,
    pub crop: SourceRect,
    pub destination: DestinationRect,
    pub transform: Transform,
    pub blend: Blend,
    pub color: ColorPipeline<'a>,
}

/// One output and its layers in bottom-to-top order.
#[derive(Clone, Copy, Debug)]
pub struct SceneRequirements<'a> {
    pub output: Extent,
    pub layers: &'a [LayerRequirements<'a>],
    pub color: OutputColor<'a>,
}

impl Blender {
    /// Check one complete scene without importing, allocating or reading images.
    ///
    /// Every source format/modifier pair is queried for DMA-BUF import and blit
    /// support. The output is queried for private intermediate storage. Color
    /// stages and geometry are checked against their compute limits. Exact
    /// memory-plane layouts and available memory remain per-image obligations.
    pub fn check_scene(&self, scene: SceneRequirements<'_>) -> io::Result<()> {
        let device: Device = self.device();
        device.check_private_image(
            nonzero(scene.output.width()),
            nonzero(scene.output.height()),
        )?;
        device.check_output_color(scene.output, scene.color)?;
        for layer in scene.layers {
            let source_width = nonzero(layer.source.extent.width());
            let source_height = nonzero(layer.source.extent.height());
            device.check_source_image(
                layer.source.format,
                source_width,
                source_height,
                layer.source.modifier,
            )?;
            device.check_private_image(source_width, source_height)?;
            device.check_color_pipeline(layer.source.extent, layer.color)?;
            self.check_geometry(
                scene.output,
                layer.source.extent,
                layer.crop,
                layer.destination,
                layer.transform,
                layer.blend,
            )?;
        }
        Ok(())
    }
}

fn nonzero(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("scene extents are nonzero")
}
