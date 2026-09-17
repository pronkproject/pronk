use std::num::NonZeroU32;

use pronk_dmabuf::Completion;

use super::*;
use crate::vulkan::test_support::readback;

fn selected_device() -> (Device, u64) {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    (Device::open(node).unwrap(), modifier)
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn imported_destination_preserves_pixels_and_independent_owners() {
    let (device, modifier) = selected_device();
    let size = NonZeroU32::new(64).unwrap();
    let source = device
        .allocate(size, size, modifier)
        .unwrap()
        .clear_and_wait([17, 85, 204])
        .unwrap()
        .0;
    let allocation = device.allocate(size, size, modifier).unwrap();
    let layout = allocation.layout();
    let fd = allocation.export().unwrap();
    drop(allocation);

    // SAFETY: The exported allocation has its exact allocator layout and no
    // owner can submit more access after its Vulkan image is dropped.
    let destination = unsafe { device.import_destination(fd, layout) }.unwrap();
    let copied = destination.copy_from_and_wait(source).unwrap();
    assert_eq!(
        copied.completion.wait_blocking().unwrap(),
        Completion::Success
    );
    let fd = copied.destination.external.fd.try_clone().unwrap();
    drop(copied.destination);

    // SAFETY: The destination copy completed successfully and released the
    // allocation in GENERAL layout before this read-only import.
    let readable = unsafe { device.import_ready_source(fd, layout) }.unwrap();
    let output = device.allocate(size, size, modifier).unwrap();
    let (output, _) = readable.copy_into_and_wait(output).unwrap();
    let (_, pixels) = readback(output);
    assert!(pixels
        .chunks_exact(4)
        .all(|pixel| pixel == [204, 85, 17, 255]));
}
