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
