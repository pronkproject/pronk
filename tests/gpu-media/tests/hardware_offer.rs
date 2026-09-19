//! Optional joint Vulkan and VA layout qualification on one selected render node.
#![cfg(feature = "native")]

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::PathBuf;

use pronk_gpu::vulkan::{Device, PackedFormat};
use pronk_media::{DrmVideoFormat, VideoCadence, VideoEncoder};

const MODES: [(u32, u32); 7] = [
    (640, 480),
    (1280, 720),
    (1366, 768),
    (1600, 900),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

fn gpu_layouts(device: &Device, width: u32, height: u32) -> BTreeSet<(u32, u64)> {
    let mut result = BTreeSet::new();
    let width = NonZeroU32::new(width).unwrap();
    let height = NonZeroU32::new(height).unwrap();
    for (format, aliases) in [
        (PackedFormat::Bgra8, [*b"AR24", *b"XR24"]),
        (PackedFormat::Rgba8, [*b"AB24", *b"XB24"]),
    ] {
        match device.output_modifiers(format, width, height) {
            Ok(modifiers) => {
                for modifier in modifiers {
                    for alias in aliases {
                        result.insert((u32::from_le_bytes(alias), modifier));
                    }
                }
            }
            Err(error) => eprintln!("{width}x{height} {format:?}: {error}"),
        }
    }
    result
}

fn describe_layouts(layouts: &BTreeSet<(u32, u64)>) -> Vec<String> {
    layouts
        .iter()
        .map(|(fourcc, modifier)| {
            format!(
                "{}:{modifier:#018x}",
                String::from_utf8_lossy(&fourcc.to_le_bytes())
            )
        })
        .collect()
}

#[test]
#[ignore = "requires an explicitly selected Vulkan and VA render node"]
fn selected_device_reports_shared_layouts_for_each_picture_size() {
    let node = PathBuf::from(
        std::env::var_os("PRONK_GPU_RENDER_NODE")
            .expect("set PRONK_GPU_RENDER_NODE to the selected render node"),
    );
    let device = Device::open(&node).unwrap();
    let encoder = VideoEncoder::va_h264(&node);
    let cadence = VideoCadence::new(NonZeroU32::new(30).unwrap(), NonZeroU32::new(1).unwrap());
    let accepted: BTreeSet<_> = encoder
        .supported_dma_buf_formats(cadence)
        .unwrap()
        .into_iter()
        .map(|DrmVideoFormat { format, modifier }| (format, modifier))
        .collect();
    let dimensions = encoder.supported_dimensions(&MODES, cadence).unwrap();
    let mut common: Option<BTreeSet<(u32, u64)>> = None;
    let mut shared_modes = Vec::new();
    for ((width, height), supported) in MODES.into_iter().zip(dimensions) {
        if !supported {
            eprintln!("{width}x{height}: VA encoder does not accept the picture size");
            continue;
        }
        let shared: BTreeSet<_> = gpu_layouts(&device, width, height)
            .intersection(&accepted)
            .copied()
            .collect();
        eprintln!("{width}x{height}: {:?}", describe_layouts(&shared));
        if !shared.is_empty() {
            shared_modes.push(format!("{width}x{height}"));
        }
        match &mut common {
            Some(common) => common.retain(|layout| shared.contains(layout)),
            None => common = Some(shared),
        }
    }
    eprintln!(
        "shared at every encoder-supported size: {:?}",
        describe_layouts(&common.unwrap_or_default())
    );
    if let Ok(expected) = std::env::var("PRONK_EXPECT_SHARED_MODE") {
        assert!(
            shared_modes.contains(&expected),
            "selected GPU and VA converter have no common layout at {expected}"
        );
    }
}
