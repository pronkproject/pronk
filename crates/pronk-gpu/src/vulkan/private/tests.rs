use super::*;
use crate::vulkan::{test_support::readback, Device};
use pronk_dmabuf::Completion;

fn device() -> (Device, u64) {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    (Device::open(node).unwrap(), modifier)
}

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_pixels_survive_independent_shared_output_reuse() {
    let (device, modifier) = device();
    let mut private = device.allocate_private(nz(31), nz(17)).unwrap();
    let mut output = device.allocate(nz(31), nz(17), modifier).unwrap();
    assert_eq!(private.extent(), (nz(31), nz(17)));
    drop(device);
    for rgb in [
        [0; 3],
        [255; 3],
        [17, 85, 204],
        [1, 127, 254],
        [255, 0, 128],
    ] {
        let filled = private.clear_waited(rgb).unwrap();
        let copied = filled.copy_into_waited(output).unwrap();
        assert_eq!(
            copied.completion.wait_blocking().unwrap(),
            Completion::Success
        );
        private = copied.source.clear_waited([77; 3]).unwrap();
        let (returned, pixels) = readback(copied.destination);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[rgb[2], rgb[1], rgb[0], 255]);
        }
        output = returned;
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_copy_rejects_uninitialized_or_mismatched_images() {
    let (device, modifier) = device();
    let private = device.allocate_private(nz(16), nz(16)).unwrap();
    let output = device.allocate(nz(16), nz(16), modifier).unwrap();
    assert_eq!(
        private.copy_into_waited(output).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput,
    );
    let private = device
        .allocate_private(nz(16), nz(16))
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
    let output = device.allocate(nz(32), nz(16), modifier).unwrap();
    assert_eq!(
        private.copy_into_waited(output).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput,
    );
    let (other, _) = self::device();
    let private = device
        .allocate_private(nz(16), nz(16))
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
    let output = other.allocate(nz(16), nz(16), modifier).unwrap();
    assert_eq!(
        private.copy_into_waited(output).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput,
    );
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_extent_limits_are_checked_before_allocation() {
    let (device, _) = device();
    assert!(device.allocate_private(nz(u32::MAX), nz(1)).is_err());
    assert!(device.allocate_private(nz(1), nz(u32::MAX)).is_err());
}
