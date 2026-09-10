//! Checked visible dispatch coordinates, independent of native image access.

use std::io;

use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    geometry::{Extent, SourceRect},
    transform::{Rotation, Transform},
};

pub(super) const PARAMETER_SIZE: usize = 13 * 4;

pub(super) struct Parameters {
    words: [u32; 13],
    pub(super) groups: [u32; 2],
}

impl Parameters {
    pub(super) fn new(
        source: Extent,
        destination: Extent,
        crop: SourceRect,
        placement: [i32; 2],
        transform: Transform,
        blend: Blend,
    ) -> io::Result<Option<Self>> {
        if crop.image() != source {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private blend crop does not match the source",
            ));
        }
        let transformed = transform.extent(crop.extent());
        let whole = SourceRect::new(transformed, [0, 0], transformed)
            .expect("whole transformed crop is valid");
        let Some(visible) = whole.clip_to(placement, destination) else {
            return Ok(None);
        };
        let mode = match blend.pixel {
            PixelBlend::None => 0,
            PixelBlend::Premultiplied => 1,
            PixelBlend::Coverage => 2,
        };
        let rotation = match transform.rotation {
            Rotation::Rotate0 => 0,
            Rotation::Rotate90 => 1,
            Rotation::Rotate180 => 2,
            Rotation::Rotate270 => 3,
        };
        let flags = rotation
            | (u32::from(transform.reflect_x) << 2)
            | (u32::from(transform.reflect_y) << 3);
        let [sx, sy] = crop.origin();
        let [tx, ty] = visible.source();
        let [dx, dy] = visible.destination();
        let width = visible.extent().width();
        let height = visible.extent().height();
        Ok(Some(Self {
            words: [
                mode,
                u32::from(blend.plane_alpha),
                sx,
                sy,
                crop.extent().width(),
                crop.extent().height(),
                tx,
                ty,
                dx,
                dy,
                width,
                height,
                flags,
            ],
            groups: [width.div_ceil(8), height.div_ceil(8)],
        }))
    }

    pub(super) fn bytes(&self) -> [u8; PARAMETER_SIZE] {
        let mut bytes = [0; PARAMETER_SIZE];
        for (word, bytes) in self.words.iter().zip(bytes.chunks_exact_mut(4)) {
            bytes.copy_from_slice(&word.to_ne_bytes());
        }
        bytes
    }
}
