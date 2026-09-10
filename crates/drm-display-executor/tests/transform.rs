use drm_display_executor::scene::{
    geometry::Extent,
    transform::{Rotation, Transform},
};

#[test]
fn quarter_turns_follow_counter_clockwise_image_order() {
    let input = Extent::new(2, 3).unwrap();
    for (rotation, size, expected) in [
        (Rotation::Rotate0, [2, 3], [1, 2, 3, 4, 5, 6]),
        (Rotation::Rotate90, [3, 2], [2, 4, 6, 1, 3, 5]),
        (Rotation::Rotate180, [2, 3], [6, 5, 4, 3, 2, 1]),
        (Rotation::Rotate270, [3, 2], [5, 3, 1, 6, 4, 2]),
    ] {
        let transform = Transform {
            rotation,
            ..Transform::default()
        };
        assert_eq!(
            transform.extent(input),
            Extent::new(size[0], size[1]).unwrap()
        );
        let actual: Vec<_> = (0..size[1])
            .flat_map(|y| {
                (0..size[0]).map(move |x| {
                    let [sx, sy] = transform.source_at(input, [x, y]).unwrap();
                    sy * 2 + sx + 1
                })
            })
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(transform.source_at(input, [size[0], 0]), None);
        assert_eq!(transform.source_at(input, [0, size[1]]), None);
    }
}

#[test]
fn reflected_transforms_are_bijections_over_non_square_images() {
    for width in 1..=5 {
        for height in 1..=5 {
            let input = Extent::new(width, height).unwrap();
            for rotation in [
                Rotation::Rotate0,
                Rotation::Rotate90,
                Rotation::Rotate180,
                Rotation::Rotate270,
            ] {
                for reflect_x in [false, true] {
                    for reflect_y in [false, true] {
                        let transform = Transform {
                            rotation,
                            reflect_x,
                            reflect_y,
                        };
                        let output = transform.extent(input);
                        let mut seen = vec![false; (width * height) as usize];
                        let unreflected = Transform {
                            rotation,
                            ..Transform::default()
                        };
                        for y in 0..output.height() {
                            for x in 0..output.width() {
                                let [sx, sy] = transform.source_at(input, [x, y]).unwrap();
                                let [ux, uy] = unreflected.source_at(input, [x, y]).unwrap();
                                assert_eq!(sx, if reflect_x { width - 1 - ux } else { ux });
                                assert_eq!(sy, if reflect_y { height - 1 - uy } else { uy });
                                let slot = &mut seen[(sy * width + sx) as usize];
                                assert!(!*slot);
                                *slot = true;
                            }
                        }
                        assert!(seen.into_iter().all(|pixel| pixel));
                    }
                }
            }
        }
    }
}

#[test]
fn reflection_precedes_rotation_at_unsigned_limits() {
    let transform = Transform {
        rotation: Rotation::Rotate90,
        reflect_x: true,
        reflect_y: false,
    };
    let source = Extent::new(u32::MAX, 2).unwrap();
    assert_eq!(transform.extent(source), Extent::new(2, u32::MAX).unwrap());
    assert_eq!(transform.source_at(source, [0, 0]), Some([0, 0]));
    assert_eq!(
        transform.source_at(source, [1, u32::MAX - 1]),
        Some([u32::MAX - 1, 1])
    );
    assert_eq!(transform.source_at(source, [0, u32::MAX]), None);
}
