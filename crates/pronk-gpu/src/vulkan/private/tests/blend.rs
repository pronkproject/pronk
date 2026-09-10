use drm_display_executor::{
    render::cpu::{
        compose::{compose, Layer},
        image::{Image as CpuImage, ImageMut, LinearLayout},
    },
    scene::{
        blend::{Blend, PixelBlend},
        format::PackedRgbFormat,
        geometry::Extent,
    },
};

use super::*;

fn reference(background: [u8; 3], layers: &[([u8; 4], Blend)]) -> [u8; 4] {
    let extent = Extent::new(1, 1).unwrap();
    let layout = LinearLayout::new(extent, PackedRgbFormat::Argb8888, 0, 4).unwrap();
    let bytes: Vec<_> = layers
        .iter()
        .map(|(rgba, _)| [rgba[2], rgba[1], rgba[0], rgba[3]])
        .collect();
    let layers: Vec<_> = bytes
        .iter()
        .zip(layers)
        .map(|(bytes, (_, blend))| {
            Layer::new(
                CpuImage::new(bytes, layout).unwrap(),
                [0, 0],
                extent,
                [0, 0],
            )
            .unwrap()
            .with_blend(*blend)
        })
        .collect();
    let mut output = [0; 4];
    compose(
        &mut ImageMut::new(&mut output, layout).unwrap(),
        background,
        &layers,
    )
    .unwrap();
    output
}

fn acquire(producer: &Device, worker: &Device, modifier: u64, rgba: [u8; 4]) -> PrivateImage {
    let source = producer
        .allocate(nz(13), nz(9), modifier)
        .unwrap()
        .clear_rgba_waited(rgba)
        .unwrap();
    // SAFETY: Identical native device identity, exact shared-image metadata and
    // submitted producer release. Source pixels remain unchanged until return.
    let imported =
        unsafe { worker.import_source(source.0.export().unwrap(), source.0.layout(), source.1) }
            .unwrap();
    let private = imported
        .copy_into_private_waited(worker.allocate_private(nz(13), nz(9)).unwrap())
        .unwrap();
    drop(source.0.clear_waited([255; 3]).unwrap());
    private
}

fn assert_output(worker: &Device, modifier: u64, image: PrivateImage, expected: [u8; 4]) {
    let shared = worker.allocate(nz(13), nz(9), modifier).unwrap();
    let copied = image.copy_into_waited(shared).unwrap();
    let (_, bytes) = readback(copied.destination);
    for pixel in bytes.chunks_exact(4) {
        for channel in 0..4 {
            assert!(
                pixel[channel].abs_diff(expected[channel]) <= 1,
                "pixel {pixel:?}, expected {expected:?}"
            );
        }
        assert_eq!(pixel[3], expected[3]);
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_blends_match_integer_reference_after_source_retirement() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let background = [21, 93, 173];
    for pixel in [
        PixelBlend::None,
        PixelBlend::Premultiplied,
        PixelBlend::Coverage,
    ] {
        for alpha in [0, 1, 127, 128, 254, 255] {
            for plane_alpha in [0, 1, 16384, 32768, 65534, 65535] {
                let rgba = [17, 85, 204, alpha];
                let blend = Blend { pixel, plane_alpha };
                let source = acquire(&producer, &worker, modifier, rgba);
                let destination = worker
                    .allocate_private(nz(13), nz(9))
                    .unwrap()
                    .clear_waited(background)
                    .unwrap();
                let result = destination.blend_waited(source, blend).unwrap();
                assert_output(
                    &worker,
                    modifier,
                    result.source,
                    [rgba[2], rgba[1], rgba[0], rgba[3]],
                );
                assert_output(
                    &worker,
                    modifier,
                    result.destination,
                    reference(background, &[(rgba, blend)]),
                );
            }
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn native_blend_stack_preserves_intermediate_color_precision() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    let background = [11, 93, 207];
    let layers = [
        (
            [91, 73, 12, 127],
            Blend {
                pixel: PixelBlend::Premultiplied,
                plane_alpha: 40123,
            },
        ),
        (
            [243, 5, 197, 53],
            Blend {
                pixel: PixelBlend::Coverage,
                plane_alpha: 19381,
            },
        ),
        (
            [3, 201, 71, 0],
            Blend {
                pixel: PixelBlend::None,
                plane_alpha: 713,
            },
        ),
    ];
    let mut destination = worker
        .allocate_private(nz(13), nz(9))
        .unwrap()
        .clear_waited(background)
        .unwrap();
    for (rgba, blend) in layers {
        let source = acquire(&producer, &worker, modifier, rgba);
        destination = destination.blend_waited(source, blend).unwrap().destination;
    }
    assert_output(
        &worker,
        modifier,
        destination,
        reference(background, &layers),
    );
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_blend_rejects_uninitialized_or_mismatched_images() {
    let (worker, _) = device();
    for initialize_source in [false, true] {
        let a = worker.allocate_private(nz(13), nz(9)).unwrap();
        let b = worker
            .allocate_private(nz(13), nz(9))
            .unwrap()
            .clear_waited([0; 3])
            .unwrap();
        let (source, destination) = if initialize_source { (b, a) } else { (a, b) };
        assert_eq!(
            destination
                .blend_waited(source, Blend::default())
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    let (other, _) = device();
    for different_device in [false, true] {
        let source = worker
            .allocate_private(nz(13), nz(9))
            .unwrap()
            .clear_waited([0; 3])
            .unwrap();
        let destination = if different_device {
            other.allocate_private(nz(13), nz(9))
        } else {
            worker.allocate_private(nz(14), nz(9))
        }
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
        assert_eq!(
            destination
                .blend_waited(source, Blend::default())
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
