//! Capture layouts that the selected renderer GPU can export and reimport.

use std::io;
use std::num::NonZeroU32;
use std::path::Path;

use pronk_backend_protocol::{DisplayMode, ModeRawLayouts, RawVideoLayout, MAX_RAW_VIDEO_LAYOUTS};
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

#[derive(Debug)]
pub(crate) struct Offers {
    pub(crate) raw_layouts: Vec<RawVideoLayout>,
    pub(crate) mode_raw_layouts: Vec<ModeRawLayouts>,
}

pub(crate) fn system_only_offer() -> Offers {
    Offers {
        raw_layouts: system_only(),
        mode_raw_layouts: Vec::new(),
    }
}

/// Advertise layouts together with the display sizes that support them.
///
/// Probing allocates, exports and reimports one disposable image per candidate.
/// The backend narrows the result against its converter. Native checks still
/// validate every buffer in the eventual capture pool.
pub(crate) fn for_modes(render_node: &Path, modes: &[DisplayMode]) -> io::Result<Offers> {
    let device = Device::open(render_node)?;
    for_modes_with(modes, |format, width, height| {
        device.output_modifiers(format, width, height)
    })
}

fn for_modes_with(
    modes: &[DisplayMode],
    mut modifiers: impl FnMut(PackedFormat, NonZeroU32, NonZeroU32) -> io::Result<Vec<u64>>,
) -> io::Result<Offers> {
    let mut mode_raw_layouts = Vec::with_capacity(modes.len());
    for mode in modes {
        let width = NonZeroU32::new(mode.width)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero output width"))?;
        let height = NonZeroU32::new(mode.height)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero output height"))?;
        let bgra = modifiers(PackedFormat::Bgra8, width, height).unwrap_or_else(|error| {
            tracing::warn!(%error, width = mode.width, height = mode.height, "BGRA output layout query failed");
            Vec::new()
        });
        let rgba = modifiers(PackedFormat::Rgba8, width, height).unwrap_or_else(|error| {
            tracing::warn!(%error, width = mode.width, height = mode.height, "RGBA output layout query failed");
            Vec::new()
        });
        let mut raw_layouts = system_only();
        'layouts: for rank in 0..bgra.len().max(rgba.len()) {
            for (fourcc, packed) in FORMATS {
                let available = match packed {
                    PackedFormat::Bgra8 => &bgra,
                    PackedFormat::Rgba8 => &rgba,
                    _ => unreachable!("the output offer contains only eight-bit packed formats"),
                };
                if let Some(&modifier) = available.get(rank) {
                    if raw_layouts.len() == MAX_RAW_VIDEO_LAYOUTS {
                        break 'layouts;
                    }
                    raw_layouts.push(RawVideoLayout::dma_buf(fourcc, modifier));
                }
            }
        }
        mode_raw_layouts.push(ModeRawLayouts {
            mode: *mode,
            raw_layouts,
        });
    }
    let mut popularity: Vec<(RawVideoLayout, usize)> = Vec::new();
    for offered in &mode_raw_layouts {
        for &layout in &offered.raw_layouts {
            if let Some((_, count)) = popularity.iter_mut().find(|(item, _)| *item == layout) {
                *count += 1;
            } else {
                popularity.push((layout, 1));
            }
        }
    }
    popularity.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let raw_layouts: Vec<_> = popularity
        .into_iter()
        .take(MAX_RAW_VIDEO_LAYOUTS)
        .map(|(layout, _)| layout)
        .collect();
    for offered in &mut mode_raw_layouts {
        offered
            .raw_layouts
            .retain(|layout| raw_layouts.contains(layout));
    }
    Ok(Offers {
        raw_layouts: if raw_layouts.is_empty() {
            system_only()
        } else {
            raw_layouts
        },
        mode_raw_layouts,
    })
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
    fn a_large_mode_does_not_hide_a_smaller_modes_gpu_layout() {
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
        let linear = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 0);
        let tiled = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9);
        assert!(result.raw_layouts.contains(&linear));
        assert!(result.raw_layouts.contains(&tiled));
        assert!(result.mode_raw_layouts[0].raw_layouts.contains(&linear));
        assert!(!result.mode_raw_layouts[1].raw_layouts.contains(&linear));
        assert!(result.mode_raw_layouts[1].raw_layouts.contains(&tiled));
    }

    #[test]
    fn keeps_system_memory_when_the_gpu_has_no_shared_layout() {
        let result = for_modes_with(&[mode(1920, 1080)], |_, _, _| Ok(vec![])).unwrap();
        assert_eq!(
            result.raw_layouts,
            vec![RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))]
        );
    }

    #[test]
    fn long_modifier_lists_leave_room_for_each_pixel_order() {
        let layouts = for_modes_with(&[mode(1920, 1080)], |_, _, _| Ok((0..80).collect())).unwrap();
        assert_eq!(layouts.raw_layouts.len(), MAX_RAW_VIDEO_LAYOUTS);
        for (fourcc, _) in FORMATS {
            assert!(layouts
                .raw_layouts
                .contains(&RawVideoLayout::dma_buf(fourcc, 0)));
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
            assert!(layouts
                .raw_layouts
                .contains(&RawVideoLayout::dma_buf(fourcc, 9)));
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
            layouts.raw_layouts,
            vec![
                RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24")),
                RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AB24"), 9),
                RawVideoLayout::dma_buf(u32::from_le_bytes(*b"XB24"), 9),
            ]
        );
    }

    #[test]
    fn a_failed_mode_probe_keeps_system_memory() {
        let layouts = for_modes_with(&[mode(1920, 1080)], |_, _, _| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "GPU device lost"))
        })
        .unwrap();
        assert_eq!(layouts.raw_layouts, system_only());
        assert_eq!(layouts.mode_raw_layouts[0].raw_layouts, system_only());
    }

    #[test]
    fn one_failed_mode_probe_does_not_hide_gpu_layouts_at_other_modes() {
        let layouts = for_modes_with(&[mode(3840, 2160), mode(1920, 1080)], |_, width, _| {
            if width.get() == 3840 {
                Err(io::Error::new(io::ErrorKind::Unsupported, "large image"))
            } else {
                Ok(vec![9])
            }
        })
        .unwrap();
        let gpu = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9);
        assert!(layouts.raw_layouts.contains(&gpu));
        assert!(!layouts.mode_raw_layouts[0].raw_layouts.contains(&gpu));
        assert!(layouts.mode_raw_layouts[1].raw_layouts.contains(&gpu));
    }

    #[test]
    #[ignore = "requires an explicitly selected Vulkan render node"]
    fn selected_gpu_reports_layouts_for_the_full_presentation_offer() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
            .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
        let modes =
            crate::preparation::initial_preparation_offer(false, &system_only()).candidate_modes;
        let layouts = for_modes(Path::new(&node), &modes).unwrap();
        eprintln!("mode output layouts: {layouts:?}");
        assert_eq!(
            layouts.raw_layouts[0],
            RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))
        );
        if let Ok(expected) = std::env::var("PRONK_GPU_MODIFIER") {
            let modifier = u64::from_str_radix(expected.trim_start_matches("0x"), 16).unwrap();
            assert!(layouts.raw_layouts.contains(&RawVideoLayout::dma_buf(
                u32::from_le_bytes(*b"AR24"),
                modifier,
            )));
        }
    }
}
