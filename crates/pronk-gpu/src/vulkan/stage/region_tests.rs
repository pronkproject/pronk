use pronk_dmabuf::Completion;
use std::num::NonZeroU32;

use drm_display_executor::{
    render::cpu::{
        compose::{compose, Layer},
        image::{Image as CpuImage, ImageMut, LinearLayout},
    },
    scene::{
        format::PackedRgbFormat,
        geometry::{Extent, SourceRect},
        transform::{Rotation, Transform},
    },
};

use super::*;
use crate::vulkan::{test_support::readback, Device, ImageLayout};

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

fn dimension(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

fn native_layout(width: u32, height: u32) -> ImageLayout {
    ImageLayout {
        width: dimension(width),
        height: dimension(height),
        modifier: 0,
        offset: 0,
        pitch: u64::from(width) * 4,
        allocation_size: u64::from(width) * u64::from(height) * 4,
    }
}

#[test]
fn native_geometry_rejects_mismatches_and_unrepresentable_offsets() {
    let source = native_layout(128, 64);
    let output = native_layout(64, 32);
    let crop = SourceRect::new(extent(128, 64), [0, 0], extent(128, 64)).unwrap();
    let copy = Copy::placed(source, output, crop, [-12, -4], [1, 2, 3]).unwrap();
    assert_eq!(
        [copy.region.src_offset.x, copy.region.src_offset.y],
        [12, 4]
    );
    assert_eq!([copy.region.dst_offset.x, copy.region.dst_offset.y], [0, 0]);
    assert_eq!(
        [copy.region.extent.width, copy.region.extent.height],
        [64, 32]
    );
    assert_eq!(copy.background, Some([1, 2, 3]));
    assert!(Copy::whole(source, output).is_err());
    assert!(Copy::placed(output, output, crop, [0, 0], [0; 3]).is_err());
    assert!(Copy::placed(source, output, crop, [64, 0], [0; 3]).is_err());
    let large = native_layout(u32::MAX, 1);
    let crop = SourceRect::new(extent(u32::MAX, 1), [1 << 31, 0], extent(1, 1)).unwrap();
    assert!(Copy::placed(large, output, crop, [0, 0], [0; 3]).is_err());
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_transformed_crops_match_the_cpu_reference_after_reuse() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let producer = Device::open(&node).unwrap();
    let worker = Device::open(&node).unwrap();
    assert_eq!(producer.identity(), worker.identity());
    let mut seed = producer
        .allocate(dimension(64), dimension(32), modifier)
        .unwrap();
    let mut input = producer
        .allocate(dimension(128), dimension(64), modifier)
        .unwrap();
    let mut staging = worker
        .allocate(dimension(96), dimension(64), modifier)
        .unwrap();
    let mut output = worker
        .allocate(dimension(96), dimension(64), modifier)
        .unwrap();
    let mut reference_source = vec![0u8; 128 * 64 * 4];
    for (index, pixel) in reference_source.chunks_exact_mut(4).enumerate() {
        let x = index % 128;
        let y = index / 128;
        pixel.copy_from_slice(if x < 64 && y < 32 {
            &[0, 0, 255, 255]
        } else {
            &[255, 0, 0, 255]
        });
    }
    let source_layout =
        LinearLayout::new(extent(128, 64), PackedRgbFormat::Argb8888, 0, 128 * 4).unwrap();
    let output_layout =
        LinearLayout::new(extent(96, 64), PackedRgbFormat::Argb8888, 0, 96 * 4).unwrap();
    for placement in [[-8, 4], [24, -8], [0, 0]] {
        for transform in blit_transforms() {
            let (ready, fence) = seed.clear_waited([255, 0, 0]).unwrap();
            seed = ready;
            // SAFETY: Exact same-device allocator metadata and completed foreign
            // release. The seed is not modified until this native read returns.
            let source =
                unsafe { producer.import_source(seed.export().unwrap(), seed.layout(), fence) }
                    .unwrap();
            let full = SourceRect::new(extent(64, 32), [0, 0], extent(64, 32)).unwrap();
            let (pattern, fence) = source
                .copy_region_into_waited(input, full, [0, 0], [0, 0, 255])
                .unwrap();
            input = pattern;
            // SAFETY: Matching physical-device/driver identities, exact compatible
            // allocator metadata and completed foreign release. Reuse follows reading.
            let source =
                unsafe { worker.import_source(input.export().unwrap(), input.layout(), fence) }
                    .unwrap();
            let crop = SourceRect::new(extent(128, 64), [16, 8], extent(96, 48)).unwrap();
            let background = [17, 85, 204];
            let (private, read_done) = staging
                .compose_opaque_waited(
                    vec![OpaqueLayer::new(source, crop, placement).with_transform(transform)],
                    background,
                )
                .unwrap();
            assert_eq!(read_done.wait_blocking().unwrap(), Completion::Success);
            input = input.clear_waited([255; 3]).unwrap().0;
            let copied = output.copy_from_waited(private).unwrap();
            staging = copied.source.clear_waited([0; 3]).unwrap().0;
            let (returned, pixels) = readback(copied.destination);
            output = returned;
            let mut expected = vec![0u8; 96 * 64 * 4];
            let source = CpuImage::new(&reference_source, source_layout).unwrap();
            let layer = Layer::new(source, [16, 8], extent(96, 48), placement)
                .unwrap()
                .with_transform(transform);
            compose(
                &mut ImageMut::new(&mut expected, output_layout).unwrap(),
                background,
                &[layer],
            )
            .unwrap();
            assert_eq!(
                pixels, expected,
                "placement {placement:?} transform {transform:?}"
            );
        }
    }
}

fn blit_transforms() -> [Transform; 8] {
    use Rotation::{Rotate0, Rotate180};
    [
        (Rotate0, false, false),
        (Rotate0, true, false),
        (Rotate0, false, true),
        (Rotate0, true, true),
        (Rotate180, false, false),
        (Rotate180, true, false),
        (Rotate180, false, true),
        (Rotate180, true, true),
    ]
    .map(|(rotation, reflect_x, reflect_y)| Transform {
        rotation,
        reflect_x,
        reflect_y,
    })
}

#[test]
fn mirrored_blit_bounds_preserve_clipped_pixel_centers() {
    let input = native_layout(128, 64);
    let output = native_layout(96, 64);
    let crop = SourceRect::new(extent(128, 64), [16, 8], extent(96, 48)).unwrap();
    let transform = Transform {
        reflect_x: true,
        ..Transform::default()
    };
    let Transfer::Blit(blit) = Transfer::placed(input, output, crop, [-8, 4], transform).unwrap()
    else {
        panic!("mirror did not select a blit");
    };
    let coordinates = |offset: ash::vk::Offset3D| [offset.x, offset.y, offset.z];
    assert_eq!(
        blit.src_offsets.map(coordinates),
        [[104, 8, 0], [16, 56, 1]]
    );
    assert_eq!(blit.dst_offsets.map(coordinates), [[0, 4, 0], [88, 52, 1]]);
    for rotation in [Rotation::Rotate90, Rotation::Rotate270] {
        let error = Transfer::placed(
            input,
            output,
            crop,
            [0, 0],
            Transform {
                rotation,
                ..transform
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
    // One-pixel reversed axes still have a nonempty nearest-neighbor footprint.
    let crop = SourceRect::new(extent(128, 64), [16, 8], extent(1, 1)).unwrap();
    let Transfer::Blit(blit) = Transfer::placed(input, output, crop, [0, 0], transform).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(blit.src_offsets.map(coordinates), [[16, 8, 0], [17, 9, 1]]);
    let large = native_layout(u32::MAX, 1);
    let crop = SourceRect::new(extent(u32::MAX, 1), [i32::MAX as u32, 0], extent(1, 1)).unwrap();
    assert!(Transfer::placed(large, output, crop, [0, 0], transform).is_err());
}
