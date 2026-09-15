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
fn native_scene_matches_ordered_reference_layers() {
    let (device, modifier) = device();
    let blender = device.create_blender().unwrap();
    let output_extent = extent(17, 11);
    let specs = [
        (
            [231, 57, 19],
            DestinationRect {
                position: [-2, 1],
                extent: extent(13, 8),
            },
            Transform::default(),
            Blend::default(),
        ),
        (
            [30, 180, 90],
            DestinationRect {
                position: [4, -1],
                extent: extent(9, 13),
            },
            Transform {
                rotation: Rotation::Rotate90,
                reflect_x: true,
                reflect_y: false,
            },
            Blend {
                pixel: PixelBlend::Coverage,
                plane_alpha: 39123,
            },
        ),
        (
            [85, 17, 204],
            DestinationRect {
                position: [11, 7],
                extent: extent(3, 2),
            },
            Transform {
                rotation: Rotation::Rotate180,
                reflect_x: false,
                reflect_y: true,
            },
            Blend {
                pixel: PixelBlend::None,
                plane_alpha: 50000,
            },
        ),
    ];
    let source_extent = extent(7, 5);
    let crop = SourceRect::new(source_extent, [1, 1], extent(5, 3)).unwrap();
    let mut private_layers = Vec::new();
    let mut source_bytes = Vec::new();
    for (rgb, destination, transform, blend) in specs {
        private_layers.push(PrivateLayer::new(
            device
                .allocate_private(nz(7), nz(5))
                .unwrap()
                .clear_and_wait(rgb)
                .unwrap(),
            crop,
            destination,
            transform,
            blend,
        ));
        source_bytes.push([rgb[2], rgb[1], rgb[0], 255].repeat(7 * 5));
    }
    let composed = blender
        .compose_and_wait(
            device.allocate_private(nz(17), nz(11)).unwrap(),
            [17, 85, 204],
            private_layers,
        )
        .unwrap();
    assert_eq!(composed.sources.len(), specs.len());
    let copied = composed
        .destination
        .copy_into_and_wait(device.allocate(nz(17), nz(11), modifier).unwrap())
        .unwrap();
    let (_, actual) = readback(copied.destination);

    let source_layout =
        LinearLayout::new(source_extent, PackedRgbFormat::Argb8888, 0, 7 * 4).unwrap();
    let layers: Vec<_> = specs
        .iter()
        .zip(&source_bytes)
        .map(|((_, destination, transform, blend), bytes)| {
            Layer::new(
                CpuImage::new(bytes, source_layout).unwrap(),
                crop.origin(),
                crop.extent(),
                destination.position,
            )
            .unwrap()
            .with_transform(*transform)
            .with_destination_extent(destination.extent)
            .with_blend(*blend)
        })
        .collect();
    let output_layout =
        LinearLayout::new(output_extent, PackedRgbFormat::Argb8888, 0, 17 * 4).unwrap();
    let mut expected = vec![0; 17 * 11 * 4];
    compose(
        &mut ImageMut::new(&mut expected, output_layout).unwrap(),
        [17, 85, 204],
        &layers,
    )
    .unwrap();
    for (actual, expected) in actual.chunks_exact(4).zip(expected.chunks_exact(4)) {
        for channel in 0..3 {
            assert!(actual[channel].abs_diff(expected[channel]) <= 1);
        }
        assert_eq!(actual[3], expected[3]);
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU selection"]
fn scene_validation_precedes_destination_initialization() {
    let (worker, _) = device();
    let (other, _) = device();
    let blender = worker.create_blender().unwrap();
    let source_extent = extent(7, 5);
    let layer = PrivateLayer::new(
        other
            .allocate_private(nz(7), nz(5))
            .unwrap()
            .clear_and_wait([0; 3])
            .unwrap(),
        SourceRect::new(source_extent, [0, 0], source_extent).unwrap(),
        DestinationRect {
            position: [0, 0],
            extent: source_extent,
        },
        Transform::default(),
        Blend::default(),
    );
    assert_eq!(
        blender
            .compose_and_wait(
                worker.allocate_private(nz(7), nz(5)).unwrap(),
                [0; 3],
                vec![layer],
            )
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn empty_scene_clears_only_a_local_destination() {
    let (worker, modifier) = device();
    let (other, _) = device();
    let blender = worker.create_blender().unwrap();
    assert_eq!(
        blender
            .compose_and_wait(
                other.allocate_private(nz(7), nz(5)).unwrap(),
                [17, 85, 204],
                Vec::new(),
            )
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    let composed = blender
        .compose_and_wait(
            worker.allocate_private(nz(7), nz(5)).unwrap(),
            [17, 85, 204],
            Vec::new(),
        )
        .unwrap();
    assert!(composed.sources.is_empty());
    let copied = composed
        .destination
        .copy_into_and_wait(worker.allocate(nz(7), nz(5), modifier).unwrap())
        .unwrap();
    let (_, pixels) = readback(copied.destination);
    assert!(pixels
        .chunks_exact(4)
        .all(|pixel| pixel == [204, 85, 17, 255]));
}
