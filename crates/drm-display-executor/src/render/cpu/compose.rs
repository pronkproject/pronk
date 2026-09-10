//! RGB reference composition with explicit plane blending and output color.

use std::collections::TryReserveError;

use super::{
    image::{Image, ImageMut},
    pixel::Rgb,
};
use crate::scene::blend::Blend;
use crate::scene::color::OutputColor;
use crate::scene::geometry::{CopyRegion, Extent, GeometryError, SourceRect};
use crate::scene::transform::Transform;

/// One integral, unscaled layer with explicit orthogonal transform and blending.
///
/// The default is premultiplied pixel alpha and fully opaque plane alpha. Source
/// pixels must be in the same encoded RGB domain as the background and output;
/// this profile performs no color-space conversion or lookup-table processing.
#[derive(Clone, Copy)]
pub struct Layer<'a> {
    image: Image<'a>,
    source: SourceRect,
    destination: [i32; 2],
    blend: Blend,
    transform: Transform,
}

impl<'a> Layer<'a> {
    pub fn new(
        image: Image<'a>,
        source: [u32; 2],
        extent: Extent,
        destination: [i32; 2],
    ) -> Result<Self, GeometryError> {
        Ok(Self {
            source: SourceRect::new(image.layout().extent(), source, extent)?,
            image,
            destination,
            blend: Blend::default(),
            transform: Transform::default(),
        })
    }

    /// Select blending without changing image encoding or validated geometry.
    pub fn with_blend(mut self, blend: Blend) -> Self {
        self.blend = blend;
        self
    }

    /// Select source-axis reflection and rotation without scaling the crop.
    pub fn with_transform(mut self, transform: Transform) -> Self {
        self.transform = transform;
        self
    }

    fn visible(&self, output: Extent) -> Option<CopyRegion> {
        let transformed = self.transform.extent(self.source.extent());
        // Region source coordinates belong to the transformed crop grid,
        // not the original image. sample() applies the inverse transform.
        SourceRect::new(transformed, [0, 0], transformed)
            .expect("complete transformed crop")
            .clip_to(self.destination, output)
    }

    fn sample(&self, transformed: [u32; 2]) -> (Rgb, u16) {
        let local = self
            .transform
            .source_at(self.source.extent(), transformed)
            .expect("visible transformed pixel");
        let [ox, oy] = self.source.origin();
        let pixels = self.image.row(oy + local[1]).expect("validated source row");
        let start = (ox + local[0]) as usize * 4;
        let source = &pixels[start..start + 4];
        Rgb::read(
            [source[0], source[1], source[2], source[3]],
            self.image.layout().format(),
        )
    }
}

/// Compose layers in bottom-to-top order over an opaque RGB background.
///
/// Components remain 16-bit normalized integers between blends and are rounded
/// to output bytes once per completed pixel. Out-of-range premultiplied sums
/// saturate. Output alpha (or its padding byte) is opaque; row padding is not
/// touched. No input is mapped or allocated here.
///
/// A single row is allocated before touching output. Allocation failure leaves
/// the output unchanged. Source reads finish before successful return; native
/// synchronization and cache maintenance, if needed, remain caller duties.
pub fn compose(
    output: &mut ImageMut<'_>,
    background: [u8; 3],
    layers: &[Layer<'_>],
) -> Result<(), TryReserveError> {
    compose_with_output_color(output, background, layers, OutputColor::default())
}

/// Compose with an explicit post-blend color stage before byte quantization.
///
/// Geometry, blending, scratch allocation and storage lifetime follow [`compose`].
/// The color stage applies to background pixels too and never changes alpha.
pub fn compose_with_output_color(
    output: &mut ImageMut<'_>,
    background: [u8; 3],
    layers: &[Layer<'_>],
    color: OutputColor<'_>,
) -> Result<(), TryReserveError> {
    let layout = output.layout();
    let extent = layout.extent();
    let mut row = Vec::new();
    // The checked output layout establishes a representable four-byte row,
    // hence its pixel count fits usize. Reservation checks scratch-byte limits.
    row.try_reserve_exact(extent.width() as usize)?;
    row.resize(extent.width() as usize, Rgb::from_bytes(background));
    for y in 0..extent.height() {
        row.fill(Rgb::from_bytes(background));
        for layer in layers {
            let Some(region) = layer.visible(extent) else {
                continue;
            };
            let [dx, dy] = region.destination();
            if y < dy || y - dy >= region.extent().height() {
                continue;
            }
            let sy = region.source()[1] + (y - dy);
            let count = region.extent().width() as usize;
            for (index, destination) in row[dx as usize..dx as usize + count].iter_mut().enumerate()
            {
                let (source, alpha) = layer.sample([region.source()[0] + index as u32, sy]);
                destination.blend(source, alpha, layer.blend);
            }
        }
        let pixels = output.row(y).expect("validated output row");
        for (destination, source) in pixels.chunks_exact_mut(4).zip(&row) {
            destination.copy_from_slice(&source.output_color(color).write(layout.format()));
        }
    }
    Ok(())
}
