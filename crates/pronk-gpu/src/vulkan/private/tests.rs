use super::*;
use crate::vulkan::{test_support::readback, Device};
use pronk_dmabuf::Completion;

mod blend;
mod formats;
mod gamma;
mod geometry;
mod pending;
mod program;
mod region;
mod ten_bit;

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
    assert!(private.allocation_size() >= 31 * 17 * 16);
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

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn imported_pixels_retire_before_private_output_is_allocated() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let mut private = worker.allocate_private(nz(31), nz(17)).unwrap();
    for value in 0..=255u8 {
        let rgba = [value, 255 - value, value ^ 0x55, value];
        let source = producer
            .allocate(nz(31), nz(17), modifier)
            .unwrap()
            .clear_rgba_waited(rgba)
            .unwrap();
        // SAFETY: Matching native devices, exact exported layout and submitted
        // producer release. The source stays unchanged until the waited read.
        let imported = unsafe {
            worker.import_source(source.0.export().unwrap(), source.0.layout(), source.1)
        }
        .unwrap();
        private = imported.copy_into_private_waited(private).unwrap();
        // No downstream image exists during source reading. Reuse and destroy
        // the producer allocation before allocating the independent output.
        drop(source.0.clear_waited([255; 3]).unwrap());
        let output = worker.allocate(nz(31), nz(17), modifier).unwrap();
        let copied = private.copy_into_waited(output).unwrap();
        copied.completion.wait_blocking().unwrap();
        private = copied.source;
        let (_, pixels) = readback(copied.destination);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_source_copy_rejects_mismatched_extents_or_devices() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    for different_device in [false, true] {
        let source = producer
            .allocate(nz(16), nz(16), modifier)
            .unwrap()
            .clear_waited([17, 85, 204])
            .unwrap();
        // SAFETY: The producer retains unchanged native pixels and the import
        // carries its exact layout and completed foreign-release dependency.
        let imported = unsafe {
            worker.import_source(source.0.export().unwrap(), source.0.layout(), source.1)
        }
        .unwrap();
        let private = if different_device {
            producer.allocate_private(nz(16), nz(16))
        } else {
            worker.allocate_private(nz(32), nz(16))
        }
        .unwrap();
        assert_eq!(
            imported
                .copy_into_private_waited(private)
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::InvalidInput,
        );
    }
}
