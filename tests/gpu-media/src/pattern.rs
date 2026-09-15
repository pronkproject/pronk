//! Frame-specific fixture pixels, separated by more than codec tolerance.

use drm_display_executor::scene::color::Lut;
use drm_display_executor::scene::geometry::{CopyRegion, Extent, SourceRect};
use pronk_gpu::vulkan::PackedFormat;

pub const FRAMES: u32 = 20;
pub const TOLERANCE: u8 = 6;
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const BACKGROUND: [u8; 3] = [0; 3];
pub const GAMMA: [[u16; 3]; 2] = [[0, 65535, 0], [65535, 0, 65535]];

/// Reference output after the fixture's green-channel inversion.
pub fn output_color(rgb: [u8; 3]) -> [u8; 3] {
    Lut::new(&GAMMA)
        .unwrap()
        .sample(rgb.map(|value| u16::from(value) * 257))
        .map(|value| ((u32::from(value) + 128) / 257) as u8)
}

pub fn source_crop() -> SourceRect {
    SourceRect::new(
        Extent::new(WIDTH, HEIGHT).unwrap(),
        [32, 16],
        Extent::new(1856, 1024).unwrap(),
    )
    .unwrap()
}

pub fn placement(sequence: u32) -> [i32; 2] {
    assert!(sequence < FRAMES, "fixture sequence exceeds color domain");
    match sequence % 3 {
        0 => [-32, 16],
        1 => [32, -16],
        _ => [64, 32],
    }
}

#[cfg(test)]
pub fn visible(sequence: u32) -> CopyRegion {
    source_crop()
        .clip_to(placement(sequence), Extent::new(WIDTH, HEIGHT).unwrap())
        .unwrap()
}

#[derive(Clone, Copy)]
pub struct Plane {
    pub format: PackedFormat,
    pub crop: SourceRect,
    pub placement: [i32; 2],
    pub color: [u8; 3],
}

pub type Scene = [Plane; 4];

impl Plane {
    pub fn visible(self) -> CopyRegion {
        self.crop
            .clip_to(self.placement, Extent::new(WIDTH, HEIGHT).unwrap())
            .unwrap()
    }
}

/// Base, overlay, cursor-sized layer and RGB565 patch, all with opaque pixels.
pub fn scene(sequence: u32) -> Scene {
    let base = Plane {
        format: PackedFormat::Bgr10A2,
        crop: source_crop(),
        placement: placement(sequence),
        color: color(sequence),
    };
    let full = |width, height| {
        let extent = Extent::new(width, height).unwrap();
        SourceRect::new(extent, [0, 0], extent).unwrap()
    };
    [
        base,
        Plane {
            format: PackedFormat::Rgba8,
            crop: full(640, 480),
            placement: [640, 320],
            color: color((sequence + 7) % FRAMES),
        },
        Plane {
            format: PackedFormat::Bgra8,
            crop: full(128, 128),
            placement: [608 + (sequence % 3) as i32 * 64, 288],
            color: color((sequence + 13) % FRAMES),
        },
        Plane {
            format: PackedFormat::Rgb565,
            crop: full(128, 64),
            placement: [32, 864],
            // Endpoint colors are exact at both source and output depths.
            color: [255, 0, 0],
        },
    ]
}

pub fn color(sequence: u32) -> [u8; 3] {
    assert!(sequence < FRAMES, "fixture sequence exceeds color domain");
    let value = sequence as u8;
    [
        value.wrapping_mul(11).wrapping_add(17),
        value.wrapping_mul(37).wrapping_add(23),
        value.wrapping_mul(71).wrapping_add(41),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn separated(a: [u8; 3], b: [u8; 3]) -> bool {
        a.into_iter()
            .zip(b)
            .any(|(a, b)| a.abs_diff(b) > 2 * TOLERANCE)
    }

    #[test]
    fn frames_and_overwrite_colors_have_disjoint_tolerance_ranges() {
        assert!(FRAMES <= u8::MAX as u32);
        for index in 0..FRAMES {
            let rgb = color(index);
            assert!(separated(rgb, [0, 0, 0]));
            assert!(separated(rgb, [255, 255, 255]));
            for earlier in 0..index {
                assert!(separated(rgb, color(earlier)));
            }
        }
    }

    #[test]
    fn output_gamma_preserves_frame_distinction_and_detects_overwrites() {
        assert_eq!(output_color([0; 3]), [0, 255, 0]);
        assert_eq!(output_color([255; 3]), [255, 0, 255]);
        assert_eq!(output_color([17, 23, 41]), [17, 232, 41]);
        for index in 0..FRAMES {
            let rgb = output_color(color(index));
            assert!(separated(rgb, color(index)), "identity color must fail");
            for corrupted in [[0; 3], [255; 3], output_color([255; 3])] {
                assert!(separated(rgb, corrupted));
            }
            for earlier in 0..index {
                assert!(separated(rgb, output_color(color(earlier))));
            }
        }
    }

    #[test]
    #[should_panic(expected = "fixture sequence exceeds color domain")]
    fn out_of_range_sequence_does_not_wrap_to_an_earlier_color() {
        color(FRAMES);
    }

    #[test]
    fn placed_crops_have_known_clipped_source_and_output_rectangles() {
        assert_eq!(source_crop().origin(), [32, 16]);
        assert_eq!(source_crop().extent(), Extent::new(1856, 1024).unwrap());
        let expected = [
            ([64, 16], [0, 16], [1824, 1024]),
            ([32, 32], [32, 0], [1856, 1008]),
            ([32, 16], [64, 32], [1856, 1024]),
        ];
        for sequence in 0..FRAMES {
            let region = visible(sequence);
            let (source, destination, size) = expected[sequence as usize % 3];
            assert_eq!(region.source(), source);
            assert_eq!(region.destination(), destination);
            assert_eq!([region.extent().width(), region.extent().height()], size);
            assert!(size[0] * size[1] < WIDTH * HEIGHT);
            assert!(destination[0] + size[0] <= WIDTH);
            assert!(destination[1] + size[1] <= HEIGHT);
        }
    }

    #[test]
    fn scene_layers_have_distinct_colors_and_known_overlap() {
        for sequence in 0..FRAMES {
            let [base, overlay, cursor, patch] = scene(sequence);
            assert_eq!(base.format, PackedFormat::Bgr10A2);
            assert_eq!(overlay.format, PackedFormat::Rgba8);
            assert_eq!(cursor.format, PackedFormat::Bgra8);
            assert_eq!(patch.format, PackedFormat::Rgb565);
            assert_eq!(patch.color, [255, 0, 0]);
            assert_eq!(patch.visible().destination(), [32, 864]);
            assert_eq!(patch.visible().extent(), Extent::new(128, 64).unwrap());
            assert!(separated(
                output_color(patch.color),
                output_color(BACKGROUND)
            ));
            assert!(separated(output_color(patch.color), output_color([255; 3])));
            assert_eq!(base.visible(), visible(sequence));
            assert_eq!(overlay.crop.image(), Extent::new(640, 480).unwrap());
            assert_eq!(overlay.visible().destination(), [640, 320]);
            assert_eq!(overlay.visible().extent(), Extent::new(640, 480).unwrap());
            assert_eq!(cursor.crop.image(), Extent::new(128, 128).unwrap());
            assert_eq!(
                cursor.visible().destination(),
                [608 + sequence % 3 * 64, 288]
            );
            assert_eq!(cursor.visible().extent(), Extent::new(128, 128).unwrap());
            assert!(separated(base.color, overlay.color));
            assert!(separated(base.color, cursor.color));
            assert!(separated(overlay.color, cursor.color));
            // The cursor intersects both the overlay and exposed base pixels.
            let [x, y] = cursor.visible().destination();
            assert!(x + 128 > 640 && x < 1280);
            assert!(y < 320 && y + cursor.visible().extent().height() > 320);
        }
    }
}
