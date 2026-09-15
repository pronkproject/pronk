use drm_display_executor::scene::color::{ColorMatrix, Lut, OutputColor};

use super::*;

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
    let native = device.create_output_color(color).unwrap();
    let mut image = device.allocate_private(nz(13), nz(5)).unwrap();
    let mut output = device.allocate(nz(13), nz(5), modifier).unwrap();
    for rgb in [[0, 0, 0], [17, 85, 204], [1, 127, 254], [255, 255, 255]] {
        image = image.clear_waited(rgb).unwrap();
        image = native.apply_waited(image).unwrap();
        let copied = image.copy_into_waited(output).unwrap();
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
    let native = worker.create_output_color(OutputColor::default()).unwrap();
    let error = match native.apply_waited(worker.allocate_private(nz(1), nz(1)).unwrap()) {
        Ok(_) => panic!("uninitialized image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let (other, _) = device();
    let foreign = other
        .allocate_private(nz(1), nz(1))
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
    let error = match native.apply_waited(foreign) {
        Ok(_) => panic!("foreign image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
