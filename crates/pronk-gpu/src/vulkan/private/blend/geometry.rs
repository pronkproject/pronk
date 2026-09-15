//! Checked visible dispatch coordinates, independent of native image access.

use std::io;

use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    geometry::{DestinationRect, Extent, SourceRect},
    transform::{Rotation, Transform},
};

pub(super) const PARAMETER_SIZE: usize = 16 * 4;

pub(super) struct Parameters {
    words: [u32; 16],
    pub(super) groups: [u32; 2],
}

impl Parameters {
    pub(super) fn new(
        source: Extent,
        destination: Extent,
        crop: SourceRect,
        placement: DestinationRect,
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
        let whole = SourceRect::new(placement.extent, [0, 0], placement.extent)
            .expect("whole destination grid is valid");
        let Some(visible) = whole.clip_to(placement.position, destination) else {
            return Ok(None);
        };
        for (source, destination) in [
            (transformed.width(), placement.extent.width()),
            (transformed.height(), placement.extent.height()),
        ] {
            // The shader evaluates (2 * pixel + 1) * source / (2 * destination).
            // Bound both numerator and denominator before any native work.
            if 2 * u64::from(destination) > u64::from(u32::MAX)
                || (2 * u64::from(destination) - 1) * u64::from(source) > u64::from(u32::MAX)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "private blend scaling exceeds integer shader limits",
                ));
            }
        }
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
                0, // Align the following uvec2 to eight bytes in GLSL.
                placement.extent.width(),
                placement.extent.height(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_rejects_overflow_before_submission() {
        let source = Extent::new(16384, 16384).unwrap();
        let crop = SourceRect::new(source, [0, 0], source).unwrap();
        for (width, accepted) in [(131072, true), (131073, false), (u32::MAX, false)] {
            let placement = DestinationRect {
                position: [-100, 0],
                extent: Extent::new(width, 1).unwrap(),
            };
            let result = Parameters::new(
                source,
                source,
                crop,
                placement,
                Transform::default(),
                Blend::default(),
            );
            if accepted {
                assert!(result.unwrap().is_some());
            } else {
                assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Unsupported);
            }
        }
    }
}
