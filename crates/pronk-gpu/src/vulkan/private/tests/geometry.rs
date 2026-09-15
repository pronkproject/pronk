use drm_display_executor::{
    render::cpu::{
        compose::{compose, Layer},
        image::{Image as CpuImage, ImageMut, LinearLayout},
    },
    scene::{
        blend::{Blend, PixelBlend},
        format::PackedRgbFormat,
        geometry::{DestinationRect, Extent, SourceRect},
        transform::{Rotation, Transform},
    },
};

use super::*;

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_cropped_blends_match_all_orthogonal_transforms() {
    check_transformed_blends(false);
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_scaled_blends_match_all_orthogonal_transforms() {
    check_transformed_blends(true);
}

fn check_transformed_blends(scaled: bool) {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let seed = producer
        .allocate(nz(7), nz(5), modifier)
        .unwrap()
        .clear_rgba_waited([231, 57, 19, 97])
        .unwrap();
    // SAFETY: Exact local native layout and completed producer release, with
    // unchanged seed pixels retained throughout the waited pattern copy.
    let imported =
        unsafe { producer.import_source(seed.0.export().unwrap(), seed.0.layout(), seed.1) }
            .unwrap();
    let full = SourceRect::new(extent(7, 5), [0, 0], extent(7, 5)).unwrap();
    let pattern = imported
        .copy_region_into_waited(
            producer.allocate(nz(32), nz(24), modifier).unwrap(),
            full,
            [4, 3],
            [30, 90, 180],
        )
        .unwrap();
    // SAFETY: Matching device identities and exact exported image metadata.
    // Pattern reuse/destruction follows completed private acquisition.
    let imported =
        unsafe { worker.import_source(pattern.0.export().unwrap(), pattern.0.layout(), pattern.1) }
            .unwrap();
    let mut source = imported
        .copy_into_private_waited(worker.allocate_private(nz(32), nz(24)).unwrap())
        .unwrap();
    drop(pattern.0.clear_waited([255; 3]).unwrap());
    drop(seed.0);
    let mut reference = vec![0; 32 * 24 * 4];
    for y in 0..24 {
        for x in 0..32 {
            reference[(y * 32 + x) * 4..][..4].copy_from_slice(
                if (4..11).contains(&x) && (3..8).contains(&y) {
                    &[19, 57, 231, 97]
                } else {
                    &[180, 90, 30, 255]
                },
            );
        }
    }
    let source_layout =
        LinearLayout::new(extent(32, 24), PackedRgbFormat::Argb8888, 0, 32 * 4).unwrap();
    let output_layout =
        LinearLayout::new(extent(17, 11), PackedRgbFormat::Argb8888, 0, 17 * 4).unwrap();
    let crop = SourceRect::new(extent(32, 24), [2, 1], extent(13, 9)).unwrap();
    let blend = Blend {
        pixel: PixelBlend::Coverage,
        plane_alpha: 39123,
    };
    let background = [17, 85, 204];
    let blender = worker.create_blender().unwrap();
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
                let sizes = if scaled {
                    vec![
                        extent(23, 17),
                        extent(26, 18),
                        extent(7, 5),
                        extent(1, 1),
                        extent(19, 3),
                    ]
                } else {
                    vec![transform.extent(crop.extent())]
                };
                for size in sizes {
                    for placement in [
                        [-3, 2],
                        [5, -2],
                        [0, 0],
                        [16, 10],
                        [i32::MIN, 0],
                        [i32::MAX, 0],
                    ] {
                        let destination = worker
                            .allocate_private(nz(17), nz(11))
                            .unwrap()
                            .clear_waited(background)
                            .unwrap();
                        let result = if scaled {
                            blender.blend_scaled_region_waited(
                                destination,
                                source,
                                crop,
                                DestinationRect {
                                    position: placement,
                                    extent: size,
                                },
                                transform,
                                blend,
                            )
                        } else {
                            destination
                                .blend_region_waited(source, crop, placement, transform, blend)
                        }
                        .unwrap();
                        source = result.source;
                        let copied = result
                            .destination
                            .copy_into_waited(worker.allocate(nz(17), nz(11), modifier).unwrap())
                            .unwrap();
                        let (_, actual) = readback(copied.destination);
                        let layer = Layer::new(
                            CpuImage::new(&reference, source_layout).unwrap(),
                            [2, 1],
                            extent(13, 9),
                            placement,
                        )
                        .unwrap()
                        .with_transform(transform)
                        .with_destination_extent(size)
                        .with_blend(blend);
                        let mut expected = vec![0; 17 * 11 * 4];
                        compose(
                            &mut ImageMut::new(&mut expected, output_layout).unwrap(),
                            background,
                            &[layer],
                        )
                        .unwrap();
                        for (actual, expected) in
                            actual.chunks_exact(4).zip(expected.chunks_exact(4))
                        {
                            for channel in 0..3 {
                                assert!(
                                    actual[channel].abs_diff(expected[channel]) <= 1,
                                    "{actual:?} != {expected:?}: {placement:?}, {size:?}, {transform:?}"
                                );
                            }
                            assert_eq!(actual[3], expected[3]);
                        }
                    }
                }
            }
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_crop_must_match_its_actual_source_image() {
    let (worker, _) = device();
    let source = worker
        .allocate_private(nz(13), nz(9))
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
    let destination = worker
        .allocate_private(nz(17), nz(11))
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
    let wrong = SourceRect::new(extent(14, 9), [0, 0], extent(13, 9)).unwrap();
    assert_eq!(
        destination
            .blend_region_waited(
                source,
                wrong,
                [0, 0],
                Transform::default(),
                Blend::default()
            )
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}
