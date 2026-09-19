//! Capture layouts that the selected renderer GPU can export and reimport.

use std::io;
use std::num::NonZeroU32;
use std::path::Path;

use pronk_backend_protocol::{DisplayMode, RawVideoLayout, MAX_RAW_VIDEO_LAYOUTS};
use pronk_gpu::vulkan::{Device, PackedFormat};

const FORMATS: [(u32, PackedFormat); 4] = [
    (u32::from_le_bytes(*b"AR24"), PackedFormat::Bgra8),
    (u32::from_le_bytes(*b"XR24"), PackedFormat::Bgra8),
    (u32::from_le_bytes(*b"AB24"), PackedFormat::Rgba8),
    (u32::from_le_bytes(*b"XB24"), PackedFormat::Rgba8),
];

pub(crate) fn system_only() -> Vec<RawVideoLayout> {
    vec![RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))]
}

/// Advertise only layouts available at every offered display size.
///
/// Probing allocates, exports and reimports one disposable image per candidate.
/// The backend narrows the result against its converter. Native checks still
/// validate every buffer in the eventual capture pool.
pub(crate) fn for_modes(
    render_node: &Path,
    modes: &[DisplayMode],
) -> io::Result<Vec<RawVideoLayout>> {
    let device = Device::open(render_node)?;
    for_modes_with(modes, |format, width, height| {
        device.output_modifiers(format, width, height)
    })
}

fn for_modes_with(
    modes: &[DisplayMode],
    mut modifiers: impl FnMut(PackedFormat, NonZeroU32, NonZeroU32) -> io::Result<Vec<u64>>,
) -> io::Result<Vec<RawVideoLayout>> {
    let mut result = system_only();
    if modes.is_empty() {
        return Ok(result);
    }
    // An unsupported channel order must not hide a usable one. Keep a GPU
    // offer only if at least one order can be queried across all modes.
    let bgra = common_modifiers(modes, PackedFormat::Bgra8, &mut modifiers);
    let rgba = common_modifiers(modes, PackedFormat::Rgba8, &mut modifiers);
    let (bgra, rgba) = match (bgra, rgba) {
        (Ok(bgra), Ok(rgba)) => (bgra, rgba),
        (Ok(bgra), Err(error)) => {
            tracing::warn!(%error, "RGBA output layout query failed");
            (bgra, Vec::new())
        }
        (Err(error), Ok(rgba)) => {
            tracing::warn!(%error, "BGRA output layout query failed");
            (Vec::new(), rgba)
        }
        (Err(error), Err(_)) => return Err(error),
    };
    // Share the bounded offer across pixel orders. One format with an unusually
    // long modifier list must not hide every layout of the other formats.
    for rank in 0..bgra.len().max(rgba.len()) {
        for (fourcc, packed) in FORMATS {
            let modifiers = match packed {
                PackedFormat::Bgra8 => &bgra,
                PackedFormat::Rgba8 => &rgba,
                _ => unreachable!("the output offer contains only eight-bit packed formats"),
            };
            if let Some(&modifier) = modifiers.get(rank) {
                if result.len() == MAX_RAW_VIDEO_LAYOUTS {
                    return Ok(result);
                }
                result.push(RawVideoLayout::dma_buf(fourcc, modifier));
            }
        }
    }
    Ok(result)
}

fn common_modifiers(
    modes: &[DisplayMode],
    format: PackedFormat,
    modifiers: &mut impl FnMut(PackedFormat, NonZeroU32, NonZeroU32) -> io::Result<Vec<u64>>,
) -> io::Result<Vec<u64>> {
    let mut common = None;
    for mode in modes {
        let width = NonZeroU32::new(mode.width)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero output width"))?;
        let height = NonZeroU32::new(mode.height)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero output height"))?;
        let available = modifiers(format, width, height)?;
        match &mut common {
            None => common = Some(available),
            Some(common) => common.retain(|modifier| available.contains(modifier)),
        }
    }
    Ok(common.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32) -> DisplayMode {
        DisplayMode {
            width,
            height,
            refresh_millihz: 60_000,
            flags: 0,
        }
    }

    #[test]
    fn advertises_only_modifiers_available_for_every_mode() {
        let result = for_modes_with(&[mode(1920, 1080), mode(3840, 2160)], |format, width, _| {
            if format == PackedFormat::Bgra8 {
                Ok(if width.get() == 1920 {
                    vec![0, 9]
                } else {
                    vec![9]
                })
            } else {
                Ok(vec![])
            }
        })
        .unwrap();
        assert_eq!(
            result,
            vec![
                RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24")),
                RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9),
                RawVideoLayout::dma_buf(u32::from_le_bytes(*b"XR24"), 9),
            ]
        );
    }

    #[test]
    fn keeps_system_memory_when_the_gpu_has_no_shared_layout() {
        let result = for_modes_with(&[mode(1920, 1080)], |_, _, _| Ok(vec![])).unwrap();
        assert_eq!(
            result,
            vec![RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))]
        );
    }

    #[test]
    fn long_modifier_lists_leave_room_for_each_pixel_order() {
        let layouts = for_modes_with(&[mode(1920, 1080)], |_, _, _| Ok((0..80).collect())).unwrap();
        assert_eq!(layouts.len(), MAX_RAW_VIDEO_LAYOUTS);
        for (fourcc, _) in FORMATS {
            assert!(layouts.contains(&RawVideoLayout::dma_buf(fourcc, 0)));
        }
    }

    #[test]
    fn fourcc_aliases_share_one_gpu_query_per_mode() {
        let mut queries = Vec::new();
        let layouts = for_modes_with(&[mode(640, 480), mode(1920, 1080)], |format, width, _| {
            queries.push((format, width.get()));
            Ok(vec![9])
        })
        .unwrap();
        assert_eq!(queries.len(), 4);
        for (fourcc, _) in FORMATS {
            assert!(layouts.contains(&RawVideoLayout::dma_buf(fourcc, 9)));
        }
    }

    #[test]
    fn one_unsupported_channel_order_does_not_hide_the_other() {
        let layouts = for_modes_with(&[mode(1920, 1080)], |format, _, _| {
            if format == PackedFormat::Bgra8 {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "BGRA unavailable",
                ))
            } else {
                Ok(vec![9])
            }
        })
        .unwrap();
        assert_eq!(
            layouts,
            vec![
                RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24")),
                RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AB24"), 9),
                RawVideoLayout::dma_buf(u32::from_le_bytes(*b"XB24"), 9),
            ]
        );
    }

    #[test]
    fn both_failed_channel_orders_preserve_the_probe_error() {
        let error = for_modes_with(&[mode(1920, 1080)], |_, _, _| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "GPU device lost"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    #[ignore = "requires an explicitly selected Vulkan render node"]
    fn selected_gpu_reports_layouts_for_the_full_presentation_offer() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
            .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
        let modes =
            crate::preparation::initial_preparation_offer(false, &system_only()).candidate_modes;
        let layouts = for_modes(Path::new(&node), &modes).unwrap();
        eprintln!("shared output layouts: {layouts:?}");
        assert_eq!(
            layouts[0],
            RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))
        );
        if let Ok(expected) = std::env::var("PRONK_GPU_MODIFIER") {
            let modifier = u64::from_str_radix(expected.trim_start_matches("0x"), 16).unwrap();
            assert!(layouts.contains(&RawVideoLayout::dma_buf(
                u32::from_le_bytes(*b"AR24"),
                modifier,
            )));
        }
    }
}
