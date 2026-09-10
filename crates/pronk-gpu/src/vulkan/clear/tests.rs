//! CPU readback is only a test oracle; the producer never maps raw pixels.

use super::*;
use crate::vulkan::test_support::readback;
use crate::vulkan::Device;
use pronk_dmabuf::Completion;
use std::num::NonZeroU32;

#[test]
fn native_failure_does_not_validate_pixels() {
    assert!(require_success(Completion::Success).is_ok());
    assert!(require_success(Completion::Failed(-5)).is_err());
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn generated_colors_survive_repeated_foreign_handoffs() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let size = NonZeroU32::new(64).unwrap();
    let mut image = device.allocate(size, size, modifier).unwrap();
    for rgb in [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [17, 85, 204],
        [0, 0, 0],
        [255, 255, 255],
    ] {
        let (rendered, completion) = image.clear_waited(rgb).unwrap();
        assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
        let (returned, pixels) = readback(rendered);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[rgb[2], rgb[1], rgb[0], 255]);
        }
        image = returned;
    }
}
