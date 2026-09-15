//! Whole-scene requirements checked without importing or allocating images.

use std::io;
use std::num::NonZeroU32;

use drm_display_executor::scene::{
    blend::Blend,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerRequirements {
    pub source: SourceRequirements,
    pub crop: SourceRect,
    pub destination: DestinationRect,
    pub transform: Transform,
    pub blend: Blend,
}

/// One output and its layers in bottom-to-top order.
#[derive(Clone, Copy, Debug)]
pub struct SceneRequirements<'a> {
    pub output: Extent,
    pub layers: &'a [LayerRequirements],
}

impl Blender {
    /// Check one complete scene without importing, allocating or reading images.
    ///
    /// Every source format/modifier pair is queried for DMA-BUF import and blit
    /// support. The output is queried for private intermediate storage, and all
    /// geometry is checked against the compositor's dispatch limits. Exact
    /// memory-plane layouts and available memory remain per-image obligations.
    pub fn check_scene(&self, scene: SceneRequirements<'_>) -> io::Result<()> {
        let device: Device = self.device();
        device.check_private_image(
            nonzero(scene.output.width()),
            nonzero(scene.output.height()),
        )?;
        for layer in scene.layers {
            device.check_source_image(
                layer.source.format,
                nonzero(layer.source.extent.width()),
                nonzero(layer.source.extent.height()),
                layer.source.modifier,
            )?;
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
