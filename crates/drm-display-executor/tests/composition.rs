use drm_display_executor::{
    render::cpu::{
        compose::{compose, Layer},
        image::{Image, ImageMut, LinearLayout},
    },
    scene::{format::PackedRgbFormat as Format, geometry::Extent},
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
