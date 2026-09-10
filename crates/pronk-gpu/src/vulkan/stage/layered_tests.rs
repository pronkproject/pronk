use std::num::NonZeroU32;
use std::os::fd::AsFd;

use drm_display_executor::{
    render::cpu::{
        compose::{compose, Layer},
        image::{Image as CpuImage, ImageMut, LinearLayout},
    },
    scene::{
        format::PackedRgbFormat,
        geometry::{Extent, SourceRect},
    },
};
use pronk_dmabuf::{Completion, SyncFile};

use crate::vulkan::{test_support::readback, Device, OpaqueLayer};

fn device() -> (Device, u64) {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
    (
        Device::open(node).unwrap(),
        u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap(),
    )
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_opaque_layers_match_reference_order_after_all_source_reuse() {
    let (device, modifier) = device();
    let size = NonZeroU32::new(64).unwrap();
    let extent = Extent::new(64, 64).unwrap();
    let crop = SourceRect::new(extent, [8, 8], Extent::new(48, 48).unwrap()).unwrap();
    let mut staging = device.allocate(size, size, modifier).unwrap();
    let mut output = device.allocate(size, size, modifier).unwrap();
    let colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let placements = [[-8, 0], [16, 16], [0, -8]];
    let cpu_layout = LinearLayout::new(extent, PackedRgbFormat::Argb8888, 0, 256).unwrap();
    let pixels: Vec<_> = colors
        .iter()
        .map(|&[r, g, b]| [b, g, r, 255].repeat(64 * 64))
        .collect();
    for order in [[0, 1, 2], [2, 1, 0]] {
        let mut originals = Vec::new();
        let mut layers = Vec::new();
        let mut reference = Vec::new();
        for index in order {
            let (image, fence) = device
                .allocate(size, size, modifier)
                .unwrap()
                .clear_waited(colors[index])
                .unwrap();
            // SAFETY: Same native device, exact allocator profile and completed
            // foreign release. Original pixels remain unchanged through reading.
            let source =
                unsafe { device.import_source(image.export().unwrap(), image.layout(), fence) }
                    .unwrap();
            layers.push(OpaqueLayer::new(source, crop, placements[index]));
            originals.push(image);
            reference.push(
                Layer::new(
                    CpuImage::new(&pixels[index], cpu_layout).unwrap(),
                    [8, 8],
                    crop.extent(),
                    placements[index],
                )
                .unwrap(),
            );
        }
        let background = [17, 85, 204];
        let (private, completion) = staging.compose_opaque_waited(layers, background).unwrap();
        assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
        for original in originals {
            drop(original.clear_waited([255; 3]).unwrap());
        }
        let copied = output.copy_from_waited(private).unwrap();
        staging = copied.source.clear_waited([0; 3]).unwrap().0;
        let (returned, actual) = readback(copied.destination);
        output = returned;
        let mut expected = vec![0; 64 * 64 * 4];
        compose(
            &mut ImageMut::new(&mut expected, cpu_layout).unwrap(),
            background,
            &reference,
        )
        .unwrap();
        assert_eq!(actual, expected, "layer order {order:?}");
    }
    let (empty, completion) = staging
        .compose_opaque_waited(Vec::new(), [17, 85, 204])
        .unwrap();
    assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
    let (_, pixels) = readback(empty);
    assert_eq!(pixels, [204, 85, 17, 255].repeat(64 * 64));
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_layer_list_rejects_duplicate_imports_of_one_allocation() {
    let (device, modifier) = device();
    let size = NonZeroU32::new(64).unwrap();
    let extent = Extent::new(64, 64).unwrap();
    let crop = SourceRect::new(extent, [0, 0], extent).unwrap();
    let (image, fence) = device
        .allocate(size, size, modifier)
        .unwrap()
        .clear_waited([255, 0, 0])
        .unwrap();
    let mut layers = Vec::new();
    for _ in 0..2 {
        let fence = SyncFile::from_fd(fence.as_fd().try_clone_to_owned().unwrap()).unwrap();
        // SAFETY: Compatible completed source, retained unchanged. Both imports
        // describe one allocation; the composition entry must reject that alias.
        let source =
            unsafe { device.import_source(image.export().unwrap(), image.layout(), fence) }
                .unwrap();
        layers.push(OpaqueLayer::new(source, crop, [0, 0]));
    }
    let destination = device.allocate(size, size, modifier).unwrap();
    let error = destination
        .compose_opaque_waited(layers, [0; 3])
        .err()
        .expect("duplicate source accepted");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let (_, pixels) = readback(image);
    assert_eq!(pixels, [0, 0, 255, 255].repeat(64 * 64));
}
