//! Ordered composition of completed private source images.

use std::io;

use drm_display_executor::scene::{
    blend::Blend,
    geometry::{DestinationRect, SourceRect},
    transform::Transform,
};

use super::{Blender, PrivateImage};

/// One completed private source and its placement in a scene.
///
/// Instances are supplied in bottom-to-top order. The layer owns its source so
/// no other operation can change its pixels during composition.
pub struct PrivateLayer {
    image: PrivateImage,
    crop: SourceRect,
    destination: DestinationRect,
    transform: Transform,
    blend: Blend,
}

impl PrivateLayer {
    pub fn new(
        image: PrivateImage,
        crop: SourceRect,
        destination: DestinationRect,
        transform: Transform,
        blend: Blend,
    ) -> Self {
        Self {
            image,
            crop,
            destination,
            transform,
            blend,
        }
    }
}

/// A completed final image and the private sources used to produce it.
pub struct ComposedScene {
    pub sources: Vec<PrivateImage>,
    pub destination: PrivateImage,
}

impl Blender {
    /// Compose completed private sources over an opaque background.
    ///
    /// Layers are blended in slice order, from bottom to top. Every predictable
    /// geometry and device error is checked before the destination is cleared.
    /// Success returns each unchanged source in the same order and one complete
    /// destination. An error consumes all images, including a destination that
    /// native work may have partly changed, so none can be mistaken for valid
    /// output or immediately reused.
    pub fn compose_waited(
        &self,
        destination: PrivateImage,
        background: [u8; 3],
        layers: Vec<PrivateLayer>,
    ) -> io::Result<ComposedScene> {
        self.check_destination(&destination)?;
        for layer in &layers {
            self.check_scaled_region(
                &destination,
                &layer.image,
                layer.crop,
                layer.destination,
                layer.transform,
                layer.blend,
            )?;
        }
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(layers.len())
            .map_err(io::Error::other)?;
        let mut destination = destination.clear_waited(background)?;
        for layer in layers {
            let result = self.blend_scaled_region_waited(
                destination,
                layer.image,
                layer.crop,
                layer.destination,
                layer.transform,
                layer.blend,
            )?;
            sources.push(result.source);
            destination = result.destination;
        }
        Ok(ComposedScene {
            sources,
            destination,
        })
    }
}
