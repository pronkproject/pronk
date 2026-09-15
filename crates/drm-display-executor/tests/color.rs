use drm_display_executor::render::cpu::{
    compose::{compose_with_output_color, Layer},
    image::{Image, ImageMut, LinearLayout},
};
use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    color::{ColorMatrix, ColorOperation, ColorPipeline, Lut, LutError, OutputColor},
    format::PackedRgbFormat,
    geometry::Extent,
};

#[test]
fn table_storage_is_nonempty_and_bounded() {
    assert_eq!(Lut::new(&[]).unwrap_err(), LutError::Empty);
    assert_eq!(
        Lut::new(&vec![[0; 3]; 65537]).unwrap_err(),
        LutError::TooLarge,
    );
    assert!(Lut::new(&vec![[0; 3]; 65536]).is_ok());
}

#[test]
fn constant_tables_and_identity_have_distinct_meanings() {
    let constant = Lut::new(&[[123, 456, 789]]).unwrap();
    let ramp = Lut::new(&[[0; 3], [65535; 3]]).unwrap();
    for value in 0..=u16::MAX {
        let rgb = [value, u16::MAX - value, value / 2];
        assert_eq!(OutputColor::default().apply(rgb), rgb);
        assert_eq!(ramp.sample(rgb), rgb);
        assert_eq!(constant.sample(rgb), [123, 456, 789]);
    }
}

#[test]
fn interpolation_matches_independent_normalized_equations() {
    for count in [2, 3, 5, 16, 257, 65536] {
        let entries: Vec<[u16; 3]> = (0..count)
            .map(|index| {
                // Deliberately nonmonotonic, with different channel slopes.
                std::array::from_fn(|channel| {
                    ((index as u64 * (7919 + channel as u64 * 17) + 371) % 65536) as u16
                })
            })
            .collect();
        let table = Lut::new(&entries).unwrap();
        assert_eq!(table.sample([0; 3]), entries[0]);
        assert_eq!(table.sample([65535; 3]), entries[count - 1]);
        for input in 0..=u16::MAX {
            let position = f64::from(input) / 65535.0 * (count - 1) as f64;
            let low = position.floor() as usize;
            let high = position.ceil() as usize;
            let fraction = position.fract();
            let expected = std::array::from_fn(|channel| {
                let a = f64::from(entries[low][channel]);
                let b = f64::from(entries[high][channel]);
                (a + (b - a) * fraction).round() as u16
            });
            assert_eq!(
                table.sample([input; 3]),
                expected,
                "{count} entries, {input}"
            );
        }
    }
}

#[test]
fn output_table_follows_blending_and_leaves_padding_untouched() {
    let source_extent = Extent::new(1, 1).unwrap();
    let source_layout = LinearLayout::new(source_extent, PackedRgbFormat::Abgr8888, 0, 4).unwrap();
    let source = Image::new(&[255, 255, 255, 128], source_layout).unwrap();
    let layer = Layer::new(source, [0, 0], source_extent, [0, 0])
        .unwrap()
        .with_blend(Blend {
            pixel: PixelBlend::Coverage,
            plane_alpha: u16::MAX,
        });
    let layout =
        LinearLayout::new(Extent::new(2, 1).unwrap(), PackedRgbFormat::Xrgb8888, 3, 12).unwrap();
    let mut bytes = [77; 15];
    compose_with_output_color(
        &mut ImageMut::new(&mut bytes, layout).unwrap(),
        [0; 3],
        &[layer],
        OutputColor {
            degamma: None,
            matrix: None,
            gamma: Some(Lut::new(&[[0; 3], [16384; 3], [65535; 3]]).unwrap()),
        },
    )
    .unwrap();
    assert_eq!(&bytes[..3], &[77; 3]);
    assert_eq!(&bytes[3..11], &[65, 65, 65, 255, 0, 0, 0, 255]);
    assert_eq!(&bytes[11..], &[77; 4]);
}

#[test]
fn table_sees_full_precision_and_transforms_uncovered_background() {
    let extent = Extent::new(1, 1).unwrap();
    let layout = LinearLayout::new(extent, PackedRgbFormat::Abgr8888, 0, 4).unwrap();
    let source = Image::new(&[255; 4], layout).unwrap();
    let layer = Layer::new(source, [0, 0], extent, [0, 0])
        .unwrap()
        .with_blend(Blend {
            pixel: PixelBlend::None,
            plane_alpha: 2,
        });
    let mut entries = vec![[0; 3]; 65536];
    entries[2] = [65535; 3];
    let mut output = [0; 4];
    compose_with_output_color(
        &mut ImageMut::new(&mut output, layout).unwrap(),
        [0; 3],
        &[layer],
        OutputColor {
            degamma: None,
            matrix: None,
            gamma: Some(Lut::new(&entries).unwrap()),
        },
    )
    .unwrap();
    assert_eq!(output, [255; 4]);
    compose_with_output_color(
        &mut ImageMut::new(&mut output, layout).unwrap(),
        [255; 3],
        &[],
        OutputColor {
            degamma: None,
            matrix: None,
            gamma: Some(Lut::new(&[[257, 514, 771]]).unwrap()),
        },
    )
    .unwrap();
    assert_eq!(output, [1, 2, 3, 255]);
}

#[test]
fn output_color_applies_degamma_matrix_then_gamma() {
    let degamma = [[1000, 2000, 3000]];
    let gamma = [[111, 222, 333], [444, 555, 666]];
    let mut coefficients = [0; 12];
    coefficients[0] = 1 << 32;
    coefficients[6] = 1 << 32;
    coefficients[9] = 1 << 32;
    assert_eq!(
        OutputColor {
            degamma: Some(Lut::new(&degamma).unwrap()),
            matrix: Some(ColorMatrix::from_sign_magnitude(coefficients)),
            gamma: Some(Lut::new(&gamma).unwrap()),
        }
        .apply([9, 8, 7]),
        [116, 237, 343]
    );
}

#[test]
fn output_matrix_clamps_signed_and_extreme_results() {
    let negative_one = (1 << 63) | (1 << 32);
    let mut negative = [0; 12];
    negative[0] = negative_one;
    negative[5] = 1 << 32;
    negative[10] = 1 << 32;
    assert_eq!(
        OutputColor {
            degamma: None,
            matrix: Some(ColorMatrix::from_sign_magnitude(negative)),
            gamma: None,
        }
        .apply([12345, 456, 789]),
        [0, 456, 789]
    );
    assert_eq!(
        OutputColor {
            degamma: None,
            matrix: Some(ColorMatrix::from_sign_magnitude([i64::MAX as u64; 12])),
            gamma: None,
        }
        .apply([u16::MAX; 3]),
        [u16::MAX; 3]
    );
}

#[test]
fn ordered_color_operations_preserve_only_matrix_extended_range() {
    let offset_magnitude = 65536_u64 << 32;
    let negative_offset = (1 << 63) | offset_magnitude;
    let offset = ColorMatrix::from_sign_magnitude([
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
    assert_eq!(
        ColorPipeline::new(&[
            ColorOperation::Matrix(offset),
            ColorOperation::Bypass,
            ColorOperation::Matrix(restore),
        ])
        .apply([123, 4567, 65535]),
        [123, 4567, 65535]
    );
    assert_eq!(
        ColorPipeline::new(&[
            ColorOperation::Matrix(offset),
            ColorOperation::SrgbInverseEotf,
            ColorOperation::Matrix(restore),
        ])
        .apply([123, 4567, 65535]),
        [65535; 3]
    );
}

#[test]
fn standard_srgb_curves_have_exact_endpoints_and_near_inverse_roundtrips() {
    let encoded_to_linear = [ColorOperation::SrgbEotf];
    let linear_to_encoded = [ColorOperation::SrgbInverseEotf];
    let roundtrip = [ColorOperation::SrgbEotf, ColorOperation::SrgbInverseEotf];
    for channel in [0, 1, 127, 1024, 2650, 4096, 32768, 65534, 65535] {
        let input = [channel; 3];
        let linear = ColorPipeline::new(&encoded_to_linear).apply(input);
        let encoded = ColorPipeline::new(&linear_to_encoded).apply(input);
        assert!(linear[0] <= channel);
        assert!(encoded[0] >= channel);
        let returned = ColorPipeline::new(&roundtrip).apply(input)[0];
        assert!(
            returned.abs_diff(channel) <= 6,
            "{channel} became {returned}"
        );
    }
    assert_eq!(ColorPipeline::new(&roundtrip).apply([0; 3]), [0; 3]);
    assert_eq!(ColorPipeline::new(&roundtrip).apply([65535; 3]), [65535; 3]);
    assert_eq!(
        ColorPipeline::new(&encoded_to_linear).apply([32768; 3]),
        [14028; 3]
    );
    assert_eq!(
        ColorPipeline::new(&linear_to_encoded).apply([32768; 3]),
        [48192; 3]
    );
}

#[test]
fn lookup_tables_clamp_prior_extended_matrix_values() {
    let table = [[100, 200, 300], [400, 500, 600]];
    let extremes = ColorMatrix::from_sign_magnitude([
        (1 << 63) | i64::MAX as u64,
        0,
        0,
        0,
        0,
        0,
        0,
        i64::MAX as u64,
        0,
        0,
        1 << 32,
        0,
    ]);
    assert_eq!(
        ColorPipeline::new(&[
            ColorOperation::Matrix(extremes),
            ColorOperation::Lut(Lut::new(&table).unwrap()),
        ])
        .apply([65535; 3]),
        [100, 500, 600]
    );
}
