use super::submit::require_available;
use pronk_dmabuf::Completion;
use std::num::NonZeroU32;

use super::*;
use crate::vulkan::{test_support::readback, Device};

#[test]
fn destination_backpressure_is_not_a_wait_or_pixel_success() {
    assert!(
        matches!(require_available(None), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    );
    assert!(require_available(Some(Completion::Failed(-5))).is_err());
    assert!(require_available(Some(Completion::Success)).is_ok());
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn imported_source_retires_before_private_and_output_reuse() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let producer = Device::open(&node).unwrap();
    let worker = Device::open(&node).unwrap();
    let size = NonZeroU32::new(64).unwrap();
    let mut source = producer.allocate(size, size, modifier).unwrap();
    let mut staging = worker.allocate(size, size, modifier).unwrap();
    let mut output = worker.allocate(size, size, modifier).unwrap();
    for rgb in [[255, 0, 0], [0, 255, 0], [0, 0, 255], [17, 85, 204]] {
        let (initialized, fence) = source.clear_waited(rgb).unwrap();
        source = initialized;
        // SAFETY: Exact allocator metadata from the same physical GPU and
        // matching Vulkan creation profile. Clear completed and released to
        // FOREIGN/GENERAL; the producer is not rewritten until the read ends.
        let imported =
            unsafe { worker.import_source(source.export().unwrap(), source.layout(), fence) }
                .unwrap();
        let (private, read_done) = imported.copy_into_waited(staging).unwrap();
        assert_eq!(read_done.wait_blocking().unwrap(), Completion::Success);
        source = source.clear_waited([255, 255, 255]).unwrap().0;
        let copied = output.copy_from_waited(private).unwrap();
        staging = copied.source.clear_waited([0, 0, 0]).unwrap().0;
        let (returned, pixels) = readback(copied.destination);
        assert!(pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [rgb[2], rgb[1], rgb[0], 255]));
        output = returned;
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn imported_backing_survives_its_exporting_device() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let producer = Device::open(&node).unwrap();
    let width = NonZeroU32::new(1920).unwrap();
    let height = NonZeroU32::new(1080).unwrap();
    let (image, fence) = producer
        .allocate(width, height, modifier)
        .unwrap()
        .clear_waited([17, 85, 204])
        .unwrap();
    let fd = image.export().unwrap();
    let layout = image.layout();
    drop(image);
    drop(producer);
    let worker = Device::open(&node).unwrap();
    // SAFETY: The exported descriptor retains the compatible same-GPU allocation
    // and its exact layout. The producer completed FOREIGN/GENERAL release and
    // was destroyed, so no writer races the import's read.
    let source = unsafe { worker.import_source(fd, layout, fence) }.unwrap();
    let (staging, _) = source
        .copy_into_waited(worker.allocate(width, height, modifier).unwrap())
        .unwrap();
    let (_, pixels) = readback(staging);
    assert!(pixels
        .chunks_exact(4)
        .all(|pixel| pixel == [204, 85, 17, 255]));
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn source_copy_rejects_aliasing_its_destination() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let size = NonZeroU32::new(64).unwrap();
    let (image, fence) = device
        .allocate(size, size, modifier)
        .unwrap()
        .clear_waited([1, 2, 3])
        .unwrap();
    // SAFETY: Same device's exact allocation metadata and completed foreign
    // release. The subsequent copy must reject the alias before any GPU access.
    let source =
        unsafe { device.import_source(image.export().unwrap(), image.layout(), fence) }.unwrap();
    assert!(
        matches!(source.copy_into_waited(image), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
}
