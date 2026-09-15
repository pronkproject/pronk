use std::io;

use drm_display_executor::scene::geometry::{Extent, SourceRect};
use drm_display_executor::scene::transform::Transform;

use super::*;
use crate::vulkan::{OpaqueLayer, PackedFormat};

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn rgba_sources_preserve_channels_through_private_bgra_output() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let mut original = producer
        .allocate_with_format(PackedFormat::Rgba8, nz(31), nz(17), modifier)
        .unwrap();
    assert_eq!(original.layout().format, PackedFormat::Rgba8);
    let mut private = worker.allocate_private(nz(31), nz(17)).unwrap();
    for value in 0..=255_u8 {
        let rgba = [value, 255 - value, value ^ 0x5a, value.wrapping_add(51)];
        let (written, producer) = original.clear_rgba_waited(rgba).unwrap();
        let (written, raw) = readback(written);
        assert!(raw.chunks_exact(4).all(|pixel| pixel == rgba));
        // SAFETY: The exported allocation has the exact reported RGBA format
        // and completed foreign GENERAL release on the matching physical GPU.
        // No original pixels are changed before the native read retires.
        let source =
            unsafe { worker.import_source(written.export().unwrap(), written.layout(), producer) }
                .unwrap();
        assert_eq!(source.layout().format, PackedFormat::Rgba8);
        private = source.copy_into_private_waited(private).unwrap();
        original = written.clear_waited([255; 3]).unwrap().0;
        let copied = private
            .copy_into_waited(worker.allocate(nz(31), nz(17), modifier).unwrap())
            .unwrap();
        private = copied.source;
        let (_, actual) = readback(copied.destination);
        let expected = [rgba[2], rgba[1], rgba[0], rgba[3]];
        assert!(
            actual.chunks_exact(4).all(|pixel| pixel == expected),
            "RGBA source {rgba:?} must become BGRA output without exchanging colors"
        );
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn byte_copies_reject_different_packed_channel_orders() {
    let (device, modifier) = device();
    let whole = Extent::new(16, 16).unwrap();
    let crop = SourceRect::new(whole, [0, 0], whole).unwrap();
    for operation in 0..4 {
        let original = device
            .allocate_with_format(PackedFormat::Rgba8, nz(16), nz(16), modifier)
            .unwrap()
            .clear_waited([17, 85, 204])
            .unwrap();
        let output = device.allocate(nz(16), nz(16), modifier).unwrap();
        let error = if operation == 0 {
            output.copy_from_waited(original.0).err().unwrap()
        } else {
            // SAFETY: An unchanged native allocation with exact RGBA metadata
            // and completed producer release is retained through rejection.
            let source = unsafe {
                device.import_source(
                    original.0.export().unwrap(),
                    original.0.layout(),
                    original.1,
                )
            }
            .unwrap();
            match operation {
                1 => source.copy_into_waited(output).err().unwrap(),
                2 => source
                    .copy_region_into_waited(output, crop, [0, 0], [0; 3])
                    .err()
                    .unwrap(),
                _ => output
                    .compose_opaque_waited(
                        vec![
                            OpaqueLayer::new(source, crop, [0, 0]).with_transform(Transform {
                                reflect_x: true,
                                ..Transform::default()
                            }),
                        ],
                        [0; 3],
                    )
                    .err()
                    .unwrap(),
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
