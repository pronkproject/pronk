//! Frame-specific fixture pixels, separated by more than codec tolerance.

use drm_display_executor::scene::geometry::{CopyRegion, Extent, SourceRect};

pub const FRAMES: u32 = 20;
pub const TOLERANCE: u8 = 6;
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const BACKGROUND: [u8; 3] = [0; 3];

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

pub fn visible(sequence: u32) -> CopyRegion {
    source_crop()
        .clip_to(placement(sequence), Extent::new(WIDTH, HEIGHT).unwrap())
        .unwrap()
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
}
