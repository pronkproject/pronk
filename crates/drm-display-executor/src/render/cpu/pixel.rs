//! Integer reference arithmetic in the selected encoded RGB domain.

use crate::scene::blend::{Blend, PixelBlend};
use crate::scene::color::OutputColor;
use crate::scene::format::PackedRgbFormat;

const MAX: u64 = u16::MAX as u64;

#[derive(Clone, Copy)]
pub(super) struct Rgb([u16; 3]);

impl Rgb {
    pub(super) fn from_bytes(rgb: [u8; 3]) -> Self {
        Self(rgb.map(|value| u16::from(value) * 257))
    }

    pub(super) fn read(bytes: [u8; 4], format: PackedRgbFormat) -> (Self, u16) {
        let rgb = match format {
            PackedRgbFormat::Xrgb8888 | PackedRgbFormat::Argb8888 => [bytes[2], bytes[1], bytes[0]],
            PackedRgbFormat::Xbgr8888 | PackedRgbFormat::Abgr8888 => [bytes[0], bytes[1], bytes[2]],
        };
        let alpha = match format {
            PackedRgbFormat::Xrgb8888 | PackedRgbFormat::Xbgr8888 => u16::MAX,
            PackedRgbFormat::Argb8888 | PackedRgbFormat::Abgr8888 => u16::from(bytes[3]) * 257,
        };
        (Self::from_bytes(rgb), alpha)
    }

    pub(super) fn blend(&mut self, source: Self, alpha: u16, blend: Blend) {
        let plane = u64::from(blend.plane_alpha);
        let alpha = match blend.pixel {
            PixelBlend::None => MAX,
            PixelBlend::Premultiplied | PixelBlend::Coverage => u64::from(alpha),
        };
        let source_weight = match blend.pixel {
            PixelBlend::Coverage => plane * alpha,
            PixelBlend::None | PixelBlend::Premultiplied => plane * MAX,
        };
        let denominator = MAX * MAX;
        let destination_weight = denominator - plane * alpha;
        for (destination, source) in self.0.iter_mut().zip(source.0) {
            // Each term is at most MAX^3; both fit u64 even for malformed
            // premultiplied pixels. Round only after applying both alphas.
            let numerator =
                u64::from(source) * source_weight + u64::from(*destination) * destination_weight;
            *destination = ((numerator + denominator / 2) / denominator).min(MAX) as u16;
        }
    }

    pub(super) fn write(self, format: PackedRgbFormat) -> [u8; 4] {
        let [r, g, b] = self.0.map(|value| ((u32::from(value) + 128) / 257) as u8);
        match format {
            PackedRgbFormat::Xrgb8888 | PackedRgbFormat::Argb8888 => [b, g, r, 255],
            PackedRgbFormat::Xbgr8888 | PackedRgbFormat::Abgr8888 => [r, g, b, 255],
        }
    }

    pub(super) fn output_color(self, color: OutputColor<'_>) -> Self {
        Self(color.apply(self.0))
    }
}
