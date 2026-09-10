use drm_display_executor::{
    render::cpu::{
        compose::{compose, Layer},
        image::{Image, ImageMut, LinearLayout},
    },
    scene::{
        blend::{Blend, PixelBlend},
        format::PackedRgbFormat as Format,
        geometry::Extent,
        transform::{Rotation, Transform},
    },
};

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

fn layout(width: u32, height: u32, format: Format) -> LinearLayout {
    LinearLayout::new(extent(width, height), format, 0, width as usize * 4).unwrap()
}

fn pixel_layer(bytes: &[u8; 4], format: Format) -> Layer<'_> {
    let image = Image::new(bytes, layout(1, 1, format)).unwrap();
    Layer::new(image, [0, 0], extent(1, 1), [0, 0]).unwrap()
}

fn render_pixel(background: [u8; 3], layers: &[Layer<'_>], format: Format) -> [u8; 4] {
    let mut result = [0; 4];
    let mut output = ImageMut::new(&mut result, layout(1, 1, format)).unwrap();
    compose(&mut output, background, layers).unwrap();
    result
}

#[test]
fn packed_formats_keep_channel_order_and_ignore_x_padding() {
    let formats = [
        Format::Xrgb8888,
        Format::Argb8888,
        Format::Xbgr8888,
        Format::Abgr8888,
    ];
    for source in formats {
        let bytes = match source {
            Format::Xrgb8888 => [204, 85, 17, 0],
            Format::Argb8888 => [204, 85, 17, 255],
            Format::Xbgr8888 => [17, 85, 204, 0],
            Format::Abgr8888 => [17, 85, 204, 255],
        };
        for destination in formats {
            let expected = match destination {
                Format::Xrgb8888 | Format::Argb8888 => [204, 85, 17, 255],
                Format::Xbgr8888 | Format::Abgr8888 => [17, 85, 204, 255],
            };
            assert_eq!(
                render_pixel([1, 2, 3], &[pixel_layer(&bytes, source)], destination),
                expected
            );
        }
    }
}

#[test]
fn premultiplied_layers_blend_bottom_to_top() {
    let red = [0, 0, 128, 128];
    assert_eq!(
        render_pixel(
            [0, 0, 255],
            &[pixel_layer(&red, Format::Argb8888)],
            Format::Argb8888
        ),
        [127, 0, 128, 255]
    );
    assert_eq!(
        render_pixel(
            [17, 85, 204],
            &[pixel_layer(&[0; 4], Format::Argb8888)],
            Format::Xrgb8888
        ),
        [204, 85, 17, 255]
    );
    let opaque_red = [0, 0, 255, 255];
    let opaque_blue = [255, 0, 0, 255];
    let layers = [
        pixel_layer(&opaque_red, Format::Argb8888),
        pixel_layer(&opaque_blue, Format::Argb8888),
    ];
    assert_eq!(render_pixel([0; 3], &layers, Format::Xrgb8888), opaque_blue);
    assert_eq!(
        render_pixel([0; 3], &[layers[1], layers[0]], Format::Xrgb8888),
        opaque_red
    );
}

#[test]
fn intermediate_precision_is_not_rounded_to_bytes_after_each_layer() {
    let black = [0, 0, 0, 102];
    let layer = pixel_layer(&black, Format::Argb8888);
    assert_eq!(
        render_pixel([1; 3], &[layer], Format::Xrgb8888),
        [1, 1, 1, 255]
    );
    assert_eq!(
        render_pixel([1; 3], &[layer, layer], Format::Xrgb8888),
        [0, 0, 0, 255]
    );
}

#[test]
fn nonpremultiplied_out_of_range_sums_saturate_instead_of_wrapping() {
    let input = [255, 255, 255, 0];
    assert_eq!(
        render_pixel(
            [255; 3],
            &[pixel_layer(&input, Format::Argb8888)],
            Format::Argb8888
        ),
        [255; 4]
    );
}

#[test]
fn plane_blend_modes_match_normalized_equations() {
    let modes = [
        PixelBlend::None,
        PixelBlend::Premultiplied,
        PixelBlend::Coverage,
    ];
    for pixel in modes {
        for plane_alpha in [0, 1, 257, 12345, 32768, 65534, 65535] {
            for alpha in 0..=255 {
                let bytes = [17, 91, 203, alpha];
                for format in [Format::Argb8888, Format::Xrgb8888] {
                    let layer =
                        pixel_layer(&bytes, format).with_blend(Blend { pixel, plane_alpha });
                    let result = render_pixel([71, 151, 239], &[layer], Format::Xrgb8888);
                    let opacity = f64::from(plane_alpha) / 65535.0;
                    let alpha = if format == Format::Xrgb8888 {
                        1.0
                    } else {
                        f64::from(alpha) / 255.0
                    };
                    let mut expected = [0, 0, 0, 255];
                    for (index, background) in [239, 151, 71].into_iter().enumerate() {
                        let source = f64::from(bytes[index]) / 255.0;
                        let background = f64::from(background) / 255.0;
                        let value = match pixel {
                            PixelBlend::None => opacity * source + (1.0 - opacity) * background,
                            PixelBlend::Premultiplied => {
                                opacity * source + (1.0 - opacity * alpha) * background
                            }
                            PixelBlend::Coverage => {
                                opacity * alpha * source + (1.0 - opacity * alpha) * background
                            }
                        };
                        let normalized = (value * 65535.0).round().clamp(0.0, 65535.0);
                        expected[index] = (normalized / 257.0).round() as u8;
                    }
                    assert_eq!(
                        result, expected,
                        "{pixel:?} plane={plane_alpha} alpha={alpha} format={format:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn pixel_interpretation_is_not_selected_by_format() {
    let bytes = [0, 0, 128, 128];
    let layer = pixel_layer(&bytes, Format::Argb8888);
    let render = |pixel| {
        render_pixel(
            [0, 0, 255],
            &[layer.with_blend(Blend {
                pixel,
                plane_alpha: u16::MAX,
            })],
            Format::Argb8888,
        )
    };
    assert_eq!(render(PixelBlend::None), [0, 0, 128, 255]);
    assert_eq!(render(PixelBlend::Premultiplied), [127, 0, 128, 255]);
    assert_eq!(render(PixelBlend::Coverage), [127, 0, 64, 255]);
    assert_eq!(
        Blend::default(),
        Blend {
            pixel: PixelBlend::Premultiplied,
            plane_alpha: u16::MAX
        }
    );
}

#[test]
fn rotated_crop_clips_in_output_space_without_reading_outside_crop() {
    let mut bytes = [0u8; 4 * 5 * 4];
    // Only the selected 2x3 crop carries red-channel labels; its border is white.
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[255; 4]);
    }
    for y in 0..3 {
        for x in 0..2 {
            let offset = ((y + 1) * 4 + x + 1) * 4;
            bytes[offset..offset + 4].copy_from_slice(&[0, 0, (y * 2 + x + 1) as u8, 255]);
        }
    }
    let source = Image::new(&bytes, layout(4, 5, Format::Argb8888)).unwrap();
    let layer = Layer::new(source, [1, 1], extent(2, 3), [-1, 1])
        .unwrap()
        .with_transform(Transform {
            rotation: Rotation::Rotate90,
            reflect_x: true,
            reflect_y: false,
        });
    let output_layout = LinearLayout::new(extent(3, 4), Format::Argb8888, 2, 16).unwrap();
    let mut result = [0x99; 62];
    compose(
        &mut ImageMut::new(&mut result, output_layout).unwrap(),
        [17, 0, 0],
        &[layer],
    )
    .unwrap();
    let view = Image::new(&result, output_layout).unwrap();
    let red: Vec<_> = (0..4)
        .flat_map(|y| view.row(y).unwrap().chunks_exact(4).map(|pixel| pixel[2]))
        .collect();
    assert_eq!(red, [17, 17, 17, 3, 5, 17, 4, 6, 17, 17, 17, 17]);
    assert_eq!(&result[..2], &[0x99; 2]);
    for start in [14, 30, 46] {
        assert_eq!(&result[start..start + 4], &[0x99; 4]);
    }
}

#[test]
fn crop_and_negative_placement_preserve_background_and_padding() {
    let mut input = [0u8; 48];
    for (index, pixel) in input.chunks_exact_mut(4).enumerate() {
        pixel.copy_from_slice(&[index as u8, 0, 0, 255]);
    }
    let image = Image::new(&input, layout(4, 3, Format::Argb8888)).unwrap();
    let layer = Layer::new(image, [1, 1], extent(3, 2), [-1, 1]).unwrap();
    let output_layout = LinearLayout::new(extent(3, 3), Format::Xrgb8888, 2, 16).unwrap();
    let mut result = [0x99u8; 50];
    let mut output = ImageMut::new(&mut result, output_layout).unwrap();
    compose(&mut output, [3, 2, 1], &[layer]).unwrap();
    let view = Image::new(&result, output_layout).unwrap();
    assert_eq!(view.row(0).unwrap(), &[1, 2, 3, 255].repeat(3));
    assert_eq!(
        view.row(1).unwrap(),
        &[6, 0, 0, 255, 7, 0, 0, 255, 1, 2, 3, 255]
    );
    assert_eq!(
        view.row(2).unwrap(),
        &[10, 0, 0, 255, 11, 0, 0, 255, 1, 2, 3, 255]
    );
    for range in [0..2, 14..18, 30..34, 46..50] {
        assert!(result[range].iter().all(|byte| *byte == 0x99));
    }
    assert!(Layer::new(image, [3, 2], extent(2, 2), [0, 0]).is_err());
}

#[test]
fn no_visible_layers_still_initializes_the_opaque_background() {
    assert_eq!(
        render_pixel([17, 85, 204], &[], Format::Argb8888),
        [204, 85, 17, 255]
    );
    let image = Image::new(&[255; 4], layout(1, 1, Format::Argb8888)).unwrap();
    let layer = Layer::new(image, [0, 0], extent(1, 1), [i32::MIN, i32::MAX]).unwrap();
    assert_eq!(
        render_pixel([17, 85, 204], &[layer], Format::Xrgb8888),
        [204, 85, 17, 255]
    );
}
