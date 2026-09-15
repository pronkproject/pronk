use drm_display_executor::scene::{
    color::{ColorMatrix, ColorOperation, ColorPipeline, Lut, OutputColor},
    geometry::Extent,
};

use super::*;

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn ordered_native_color_pipeline_matches_the_portable_reference() {
    let (device, modifier) = device();
    assert!(device.supports_shader_int64());
    let mut coefficients = [0; 12];
    coefficients[2] = 1 << 32;
    coefficients[5] = 1 << 32;
    coefficients[8] = 1 << 32;
    let offset_magnitude = 65536_u64 << 32;
    let negative_offset = (1 << 63) | offset_magnitude;
    let subtract = ColorMatrix::from_sign_magnitude([
        1 << 32,
        0,
        0,
        negative_offset,
        0,
        1 << 32,
        0,
        negative_offset,
        0,
        0,
        1 << 32,
        negative_offset,
    ]);
    let restore = ColorMatrix::from_sign_magnitude([
        1 << 32,
        0,
        0,
        offset_magnitude,
        0,
        1 << 32,
        0,
        offset_magnitude,
        0,
        0,
        1 << 32,
        offset_magnitude,
    ]);
    let table = [[1024, 2048, 4096], [32768; 3], [65535; 3]];
    let operations = [
        ColorOperation::Bypass,
        ColorOperation::SrgbEotf,
        ColorOperation::Matrix(subtract),
        ColorOperation::Bypass,
        ColorOperation::Matrix(restore),
        ColorOperation::Matrix(ColorMatrix::from_sign_magnitude(coefficients)),
        ColorOperation::SrgbInverseEotf,
        ColorOperation::Lut(Lut::new(&table).unwrap()),
    ];
    let color = ColorPipeline::new(&operations);
    let native = device.create_color_pipeline(extent(11, 7), color).unwrap();
    let mut image = device.allocate_private(nz(11), nz(7)).unwrap();
    let mut output = device.allocate(nz(11), nz(7), modifier).unwrap();
    for rgb in [[0, 0, 0], [17, 85, 204], [1, 127, 254], [255, 255, 255]] {
        image = image.clear_and_wait(rgb).unwrap();
        image = native.apply_and_wait(image).unwrap();
        let copied = image.copy_into_and_wait(output).unwrap();
        image = copied.source;
        let expected = color
            .apply(rgb.map(|value| u16::from(value) * 257))
            .map(|value| ((u32::from(value) + 128) / 257) as u8);
        let (returned, pixels) = readback(copied.destination);
        for pixel in pixels.chunks_exact(4) {
            for (actual, expected) in pixel[..3]
                .iter()
                .zip([expected[2], expected[1], expected[0]])
            {
                assert!(actual.abs_diff(expected) <= 1, "{pixel:?} != {expected:?}");
            }
            assert_eq!(pixel[3], 255);
        }
        output = returned;
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn complete_output_color_matches_the_integer_reference() {
    let (device, modifier) = device();
    assert!(device.supports_shader_int64());
    let degamma_entries = [[0, 0, 0], [8192, 16384, 32768], [65535; 3]];
    let gamma_entries = [[4096, 2048, 1024], [32768; 3], [65535; 3]];
    let matrix = ColorMatrix::from_sign_magnitude([
        0,
        1 << 32,
        0,
        1 << 30,
        1 << 32,
        0,
        0,
        0,
        0,
        0,
        (1 << 32) | (1 << 31),
        1 << 63 | 1 << 30,
    ]);
    let color = OutputColor {
        degamma: Some(Lut::new(&degamma_entries).unwrap()),
        matrix: Some(matrix),
        gamma: Some(Lut::new(&gamma_entries).unwrap()),
    };
    let native = device.create_output_color(extent(13, 5), color).unwrap();
    let mut image = device.allocate_private(nz(13), nz(5)).unwrap();
    let mut output = device.allocate(nz(13), nz(5), modifier).unwrap();
    for rgb in [[0, 0, 0], [17, 85, 204], [1, 127, 254], [255, 255, 255]] {
        image = image.clear_and_wait(rgb).unwrap();
        image = native.apply_and_wait(image).unwrap();
        let copied = image.copy_into_and_wait(output).unwrap();
        image = copied.source;
        let expected = color
            .apply(rgb.map(|value| u16::from(value) * 257))
            .map(|value| ((u32::from(value) + 128) / 257) as u8);
        let (returned, pixels) = readback(copied.destination);
        assert!(pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [expected[2], expected[1], expected[0], 255]));
        output = returned;
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU selection"]
fn empty_output_color_still_rejects_unready_or_foreign_images() {
    let (worker, _) = device();
    let native = worker
        .create_output_color(extent(1, 1), OutputColor::default())
        .unwrap();
    let error = match native.apply_and_wait(worker.allocate_private(nz(1), nz(1)).unwrap()) {
        Ok(_) => panic!("uninitialized image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let wrong_extent = worker
        .allocate_private(nz(2), nz(1))
        .unwrap()
        .clear_and_wait([0; 3])
        .unwrap();
    let error = match native.apply_and_wait(wrong_extent) {
        Ok(_) => panic!("wrong-sized image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let (other, _) = device();
    let foreign = other
        .allocate_private(nz(1), nz(1))
        .unwrap()
        .clear_and_wait([0; 3])
        .unwrap();
    let error = match native.apply_and_wait(foreign) {
        Ok(_) => panic!("foreign image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
