use drm_display_executor::scene::color::{ColorMatrix, OutputColor};

use super::*;

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn output_matrices_match_the_integer_reference() {
    let (device, modifier) = device();
    assert!(device.supports_shader_int64());
    let identity = {
        let mut values = [0; 12];
        values[0] = 1 << 32;
        values[5] = 1 << 32;
        values[10] = 1 << 32;
        values
    };
    let swapped = {
        let mut values = [0; 12];
        values[2] = 1 << 32;
        values[5] = 1 << 32;
        values[8] = 1 << 32;
        values
    };
    let negative = {
        let mut values = identity;
        values[0] |= 1 << 63;
        values
    };
    let mut image = device.allocate_private(nz(9), nz(7)).unwrap();
    let mut output = device.allocate(nz(9), nz(7), modifier).unwrap();
    for coefficients in [
        identity,
        swapped,
        negative,
        [u64::MAX; 12],
        [i64::MAX as u64; 12],
    ] {
        let matrix = ColorMatrix::from_sign_magnitude(coefficients);
        let native = device.create_output_matrix(matrix).unwrap();
        for rgb in [[0, 0, 0], [17, 85, 204], [1, 127, 254], [255, 255, 255]] {
            image = image.clear_and_wait(rgb).unwrap();
            image = native.apply_and_wait(image).unwrap();
            let copied = image.copy_into_and_wait(output).unwrap();
            image = copied.source;
            let expected = OutputColor {
                degamma: None,
                matrix: Some(matrix),
                gamma: None,
            }
            .apply(rgb.map(|value| u16::from(value) * 257))
            .map(|value| ((u32::from(value) + 128) / 257) as u8);
            let (returned, pixels) = readback(copied.destination);
            assert!(pixels
                .chunks_exact(4)
                .all(|pixel| pixel == [expected[2], expected[1], expected[0], 255]));
            output = returned;
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU selection"]
fn output_matrix_rejects_uninitialized_or_foreign_images() {
    let (worker, _) = device();
    let matrix = worker
        .create_output_matrix(ColorMatrix::from_sign_magnitude([0; 12]))
        .unwrap();
    let error = match matrix.apply_and_wait(worker.allocate_private(nz(1), nz(1)).unwrap()) {
        Ok(_) => panic!("uninitialized image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let (other, _) = device();
    let foreign = other
        .allocate_private(nz(1), nz(1))
        .unwrap()
        .clear_and_wait([0; 3])
        .unwrap();
    let error = match matrix.apply_and_wait(foreign) {
        Ok(_) => panic!("foreign image was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
