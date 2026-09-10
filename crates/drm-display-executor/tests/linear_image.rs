use drm_display_executor::{
    render::cpu::image::{Image, ImageError, ImageMut, LinearLayout},
    scene::{format::PackedRgbFormat, geometry::Extent},
};

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

#[test]
fn rows_preserve_padding_and_exclude_a_trailing_stride_requirement() {
    let layout = LinearLayout::new(extent(2, 2), PackedRgbFormat::Xrgb8888, 3, 12).unwrap();
    assert_eq!(layout.required_bytes(), 23);
    assert_eq!(layout.extent(), extent(2, 2));
    assert_eq!(layout.format(), PackedRgbFormat::Xrgb8888);
    let mut bytes = [0x55; 27];
    {
        let mut image = ImageMut::new(&mut bytes, layout).unwrap();
        assert_eq!(image.layout(), layout);
        image.row(0).unwrap().fill(0x11);
        image.row(1).unwrap().fill(0x22);
        assert!(image.row(2).is_none());
        assert!(image.row(u32::MAX).is_none());
    }
    assert_eq!(&bytes[..3], &[0x55; 3]);
    assert_eq!(&bytes[3..11], &[0x11; 8]);
    assert_eq!(&bytes[11..15], &[0x55; 4]);
    assert_eq!(&bytes[15..23], &[0x22; 8]);
    assert_eq!(&bytes[23..], &[0x55; 4]);
    let image = Image::new(&bytes[..23], layout).unwrap();
    assert_eq!(image.layout(), layout);
    assert_eq!(image.row(0).unwrap(), &[0x11; 8]);
    assert_eq!(image.row(1).unwrap(), &[0x22; 8]);
    assert!(image.row(2).is_none());
    assert!(image.row(u32::MAX).is_none());
}

#[test]
fn invalid_address_ranges_never_construct_a_view() {
    let format = PackedRgbFormat::Argb8888;
    assert_eq!(
        LinearLayout::new(extent(2, 1), format, 0, 7),
        Err(ImageError::ShortStride)
    );
    for (size, offset, stride) in [
        (extent(1, 1), usize::MAX, 4),
        (extent(1, 2), 0, usize::MAX),
        (extent(1, 3), 0, usize::MAX / 2 + 1),
    ] {
        assert_eq!(
            LinearLayout::new(size, format, offset, stride),
            Err(ImageError::AddressOverflow)
        );
    }
    let layout = LinearLayout::new(extent(2, 2), format, 3, 12).unwrap();
    let mut short = [0; 22];
    assert!(matches!(
        Image::new(&short, layout),
        Err(ImageError::ShortBuffer)
    ));
    assert!(matches!(
        ImageMut::new(&mut short, layout),
        Err(ImageError::ShortBuffer)
    ));
    let one_row = LinearLayout::new(extent(1, 1), format, 0, usize::MAX).unwrap();
    assert_eq!(
        Image::new(&[1, 2, 3, 4], one_row).unwrap().row(0),
        Some(&[1, 2, 3, 4][..])
    );
}

#[test]
fn each_packed_format_uses_four_visible_bytes_per_pixel() {
    for format in [
        PackedRgbFormat::Xrgb8888,
        PackedRgbFormat::Argb8888,
        PackedRgbFormat::Xbgr8888,
        PackedRgbFormat::Abgr8888,
    ] {
        let layout = LinearLayout::new(extent(3, 2), format, 0, 12).unwrap();
        assert_eq!(layout.required_bytes(), 24);
        assert_eq!(
            Image::new(&[0; 24], layout).unwrap().row(1).unwrap().len(),
            12
        );
    }
}
