//! Frame-specific fixture pixels, separated by more than codec tolerance.

pub const FRAMES: u32 = 20;
pub const TOLERANCE: u8 = 6;

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
}
