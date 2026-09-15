use std::io;

use drm_display_executor::scene::geometry::{Extent, SourceRect};

use super::*;
use crate::vulkan::PackedFormat;

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn cropped_private_reads_scale_and_initialize_every_destination_pixel() {
    exercise_region_reads(PackedFormat::Bgra8);
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn cropped_rgba_reads_convert_channels_while_scaling_and_padding() {
    exercise_region_reads(PackedFormat::Rgba8);
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn cropped_ten_bit_reads_preserve_placement_alpha_and_background() {
    for format in [PackedFormat::Bgr10A2, PackedFormat::Rgb10A2] {
        exercise_region_reads(format);
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn cropped_rgb565_reads_scale_into_opaque_padded_outputs() {
    exercise_region_reads(PackedFormat::Rgb565);
}

fn exercise_region_reads(format: PackedFormat) {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let source_size = extent(32, 24);
    let output_size = extent(33, 35);
    let (inner, outer) = match format {
        // Thirds are exactly representable in both eight- and ten-bit storage,
        // including the two-bit alpha channel. Geometry has no quantization
        // tolerance that could conceal a channel-order or placement error.
        PackedFormat::Bgr10A2 | PackedFormat::Rgb10A2 => ([255, 0, 85, 85], [0, 85, 170]),
        PackedFormat::Rgb565 => ([255, 0, 0, 255], [0, 255, 255]),
        _ => ([231, 57, 19, 97], [30, 90, 180]),
    };
    let background = [17, 85, 204];
    let mut private = worker.allocate_private(nz(33), nz(35)).unwrap();

    for (origin, crop_size, position, target_size) in [
        ([2, 1], extent(13, 9), [3, 4], extent(26, 27)),
        ([3, 2], extent(21, 15), [2, 3], extent(7, 5)),
        ([2, 1], extent(13, 9), [7, 2], extent(13, 9)),
        ([3, 4], extent(11, 7), [0, 0], output_size),
    ] {
        let seed = producer
            .allocate_with_format(format, nz(7), nz(5), modifier)
            .unwrap()
            .clear_rgba_waited(inner)
            .unwrap();
        // SAFETY: Exact local native layout and completed foreign release.
        // The seed is unchanged until the pattern copy completes.
        let imported =
            unsafe { producer.import_source(seed.0.export().unwrap(), seed.0.layout(), seed.1) }
                .unwrap();
        let full = SourceRect::new(extent(7, 5), [0, 0], extent(7, 5)).unwrap();
        let pattern = imported
            .copy_region_into_waited(
                producer
                    .allocate_with_format(format, nz(32), nz(24), modifier)
                    .unwrap(),
                full,
                [4, 3],
                outer,
            )
            .unwrap();
        drop(seed.0);
        // SAFETY: Matching physical GPUs, exact exported metadata and completed
        // producer release. Pixel reuse happens only after the submitted read.
        let source = unsafe {
            worker.import_source(pattern.0.export().unwrap(), pattern.0.layout(), pattern.1)
        }
        .unwrap();
        let crop = SourceRect::new(source_size, origin, crop_size).unwrap();
        private = private.clear_waited([255; 3]).unwrap();
        let pending = source
            .submit_private_region(private, crop, position, target_size, background)
            .unwrap();
        private = pending.wait().unwrap();
        // Downstream allocation and reuse have no part in the source read.
        drop(pattern.0.clear_waited([255; 3]).unwrap());
        let copied = private
            .copy_into_waited(worker.allocate(nz(33), nz(35), modifier).unwrap())
            .unwrap();
        private = copied.source;
        let (_, actual) = readback(copied.destination);
        for y in 0..output_size.height() {
            for x in 0..output_size.width() {
                let in_region = x >= position[0]
                    && y >= position[1]
                    && x - position[0] < target_size.width()
                    && y - position[1] < target_size.height();
                let expected = if in_region {
                    // These integer ratios sample away from texel boundaries,
                    // so nearest filtering has an unambiguous reference pixel.
                    let sx = origin[0]
                        + ((2 * (x - position[0]) + 1) * crop_size.width())
                            / (2 * target_size.width());
                    let sy = origin[1]
                        + ((2 * (y - position[1]) + 1) * crop_size.height())
                            / (2 * target_size.height());
                    if (4..11).contains(&sx) && (3..8).contains(&sy) {
                        [inner[2], inner[1], inner[0], inner[3]]
                    } else {
                        [outer[2], outer[1], outer[0], 255]
                    }
                } else {
                    [background[2], background[1], background[0], 255]
                };
                let index = ((y * output_size.width() + x) * 4) as usize;
                assert_eq!(
                    &actual[index..index + 4],
                    &expected,
                    "pixel ({x}, {y}), origin {origin:?}, size {target_size:?}"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn invalid_private_regions_never_produce_partial_pixels() {
    let (device, modifier) = device();
    for (crop, position, size) in [
        (
            SourceRect::new(extent(17, 16), [0, 0], extent(16, 16)).unwrap(),
            [0, 0],
            extent(16, 16),
        ),
        (
            SourceRect::new(extent(16, 16), [0, 0], extent(16, 16)).unwrap(),
            [1, 0],
            extent(16, 16),
        ),
    ] {
        let source = device
            .allocate(nz(16), nz(16), modifier)
            .unwrap()
            .clear_waited([17, 85, 204])
            .unwrap();
        // SAFETY: The unchanged local native image and completed producer
        // release are retained throughout import and the rejected operation.
        let imported = unsafe {
            device.import_source(source.0.export().unwrap(), source.0.layout(), source.1)
        }
        .unwrap();
        let private = device.allocate_private(nz(16), nz(16)).unwrap();
        assert_eq!(
            imported
                .submit_private_region(private, crop, position, size, [0; 3])
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
