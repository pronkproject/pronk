use std::num::NonZeroU32;

use pronk_dmabuf::Completion;

use super::*;
use crate::vulkan::{test_support::readback, Device};

fn selected_device() -> (Device, u64) {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    (Device::open(node).unwrap(), modifier)
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_image_copies_survive_source_rewrite() {
    let (device, modifier) = selected_device();
    let size = NonZeroU32::new(64).unwrap();
    let mut source = device.allocate(size, size, modifier).unwrap();
    let mut destination = device.allocate(size, size, modifier).unwrap();
    for rgb in [[255, 0, 0], [0, 255, 0], [0, 0, 255], [17, 85, 204]] {
        source = source.clear_waited(rgb).unwrap().0;
        let copied = destination.copy_from_waited(source).unwrap();
        assert_eq!(
            copied.completion.wait_blocking().unwrap(),
            Completion::Success
        );
        source = copied.source.clear_waited([255, 255, 255]).unwrap().0;
        let (returned, pixels) = readback(copied.destination);
        assert!(pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [rgb[2], rgb[1], rgb[0], 255]));
        destination = returned;
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn copies_reject_undefined_sources_or_incompatible_devices_and_extents() {
    let (device, modifier) = selected_device();
    let size = NonZeroU32::new(64).unwrap();
    let allocate = || device.allocate(size, size, modifier).unwrap();
    assert!(
        matches!(allocate().copy_from_waited(allocate()), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
    let initialized = || allocate().clear_waited([1, 2, 3]).unwrap().0;
    let other_size = NonZeroU32::new(32).unwrap();
    let destination = device.allocate(other_size, size, modifier).unwrap();
    assert!(
        matches!(destination.copy_from_waited(initialized()), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
    let (other, _) = selected_device();
    let destination = other.allocate(size, size, modifier).unwrap();
    assert!(
        matches!(destination.copy_from_waited(initialized()), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
}
