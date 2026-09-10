use drm_display_executor::scene::geometry::{Extent, GeometryError, SourceRect};

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

#[test]
fn empty_or_out_of_bounds_sources_are_rejected() {
    assert_eq!(Extent::new(0, 1), Err(GeometryError::EmptyExtent));
    assert_eq!(Extent::new(1, 0), Err(GeometryError::EmptyExtent));
    for origin in [[4, 0], [0, 4], [u32::MAX, 0], [0, u32::MAX]] {
        assert_eq!(
            SourceRect::new(extent(4, 4), origin, extent(1, 1)),
            Err(GeometryError::SourceOutsideImage)
        );
    }
    assert!(SourceRect::new(extent(4, 4), [1, 1], extent(4, 4)).is_err());
    assert!(SourceRect::new(extent(4, 4), [1, 1], extent(3, 3)).is_ok());
}

#[test]
fn clipping_advances_the_source_without_moving_visible_pixels() {
    let source = SourceRect::new(extent(20, 20), [7, 9], extent(8, 7)).unwrap();
    let region = source.clip_to([-3, -2], extent(4, 4)).unwrap();
    assert_eq!(region.source(), [10, 11]);
    assert_eq!(region.destination(), [0, 0]);
    assert_eq!(region.extent(), extent(4, 4));
    assert_eq!(source.image(), extent(20, 20));
    assert_eq!(source.origin(), [7, 9]);
    assert_eq!(source.extent(), extent(8, 7));
    for placement in [[-8, 0], [0, -7], [4, 0], [0, 4], [i32::MIN, i32::MAX]] {
        assert_eq!(source.clip_to(placement, extent(4, 4)), None);
    }
}

#[test]
fn endpoint_math_preserves_extreme_unsigned_extents() {
    let largest = extent(u32::MAX, u32::MAX);
    let source = SourceRect::new(largest, [0, 0], largest).unwrap();
    let clipped = source.clip_to([i32::MIN, i32::MIN], largest).unwrap();
    assert_eq!(clipped.source(), [1 << 31, 1 << 31]);
    assert_eq!(clipped.destination(), [0, 0]);
    assert_eq!(clipped.extent(), extent(i32::MAX as u32, i32::MAX as u32));
    let clipped = source.clip_to([i32::MAX, i32::MAX], largest).unwrap();
    assert_eq!(clipped.source(), [0, 0]);
    assert_eq!(clipped.destination(), [i32::MAX as u32; 2]);
    assert_eq!(clipped.extent(), extent(1 << 31, 1 << 31));
}

#[test]
fn small_crops_match_a_per_pixel_visibility_oracle() {
    let image = extent(4, 3);
    let output = extent(3, 4);
    for sx in 0..image.width() {
        for sy in 0..image.height() {
            for width in 1..=image.width() - sx {
                for height in 1..=image.height() - sy {
                    let source = SourceRect::new(image, [sx, sy], extent(width, height)).unwrap();
                    for dx in -5..=5 {
                        for dy in -5..=5 {
                            let mut expected = Vec::new();
                            for y in 0..height {
                                for x in 0..width {
                                    let ox = dx + x as i32;
                                    let oy = dy + y as i32;
                                    if ox >= 0 && oy >= 0 && ox < 3 && oy < 4 {
                                        expected.push(([sx + x, sy + y], [ox as u32, oy as u32]));
                                    }
                                }
                            }
                            let mut actual = Vec::new();
                            if let Some(region) = source.clip_to([dx, dy], output) {
                                for y in 0..region.extent().height() {
                                    for x in 0..region.extent().width() {
                                        actual.push((
                                            [region.source()[0] + x, region.source()[1] + y],
                                            [
                                                region.destination()[0] + x,
                                                region.destination()[1] + y,
                                            ],
                                        ));
                                    }
                                }
                            }
                            assert_eq!(actual, expected, "{source:?} at {dx},{dy}");
                        }
                    }
                }
            }
        }
    }
}
