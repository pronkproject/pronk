//! Initial identity-color, premultiplied RGB reference composition.

use std::collections::TryReserveError;

use super::{
    image::{Image, ImageMut},
    pixel::Rgb,
};
use crate::scene::geometry::{Extent, GeometryError, SourceRect};

/// One integral, unscaled, unrotated layer with a fully opaque plane-wide alpha.
///
/// Formats carrying pixel alpha are interpreted as premultiplied. Source
/// pixels must be in the same encoded RGB domain as the background and output;
/// this profile performs no color-space conversion or lookup-table processing.
#[derive(Clone, Copy)]
pub struct Layer<'a> {
    image: Image<'a>,
    source: SourceRect,
    destination: [i32; 2],
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
        })
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
            let Some(region) = layer.source.clip_to(layer.destination, extent) else {
                continue;
            };
            let [dx, dy] = region.destination();
            if y < dy || y - dy >= region.extent().height() {
                continue;
            }
            let sy = region.source()[1] + (y - dy);
            let pixels = layer.image.row(sy).expect("validated source row");
            let start = region.source()[0] as usize * 4;
            let count = region.extent().width() as usize;
            let pixels = &pixels[start..start + count * 4];
            for (destination, source) in row[dx as usize..dx as usize + count]
                .iter_mut()
                .zip(pixels.chunks_exact(4))
            {
                let (source, alpha) = Rgb::read(
                    [source[0], source[1], source[2], source[3]],
                    layer.image.layout().format(),
                );
                destination.blend_premultiplied(source, alpha);
            }
        }
        let pixels = output.row(y).expect("validated output row");
        for (destination, source) in pixels.chunks_exact_mut(4).zip(&row) {
            destination.copy_from_slice(&source.write(layout.format()));
        }
    }
    Ok(())
}
