//! Checked adaptation of one claimed CastKMS source into Vulkan ownership.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use castkms_renderer::FormatModifier;
use castkms_sys::{
    DRM_FORMAT_ABGR2101010, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_RGB565, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XRGB8888,
};
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{Device, ImageLayout, PackedFormat, SourceImage};

/// Whether the imported fourth channel contains alpha or ignored padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceAlpha {
    /// The fourth component is padding, or the format has no alpha component.
    Opaque,
    /// The native fourth component contains normalized pixel alpha.
    Channel,
}

pub(crate) fn import_image(
    device: &Device,
    source: &castkms_renderer::SourceImage,
    acquire_fence: Option<BorrowedFd<'_>>,
) -> io::Result<(SourceImage, SourceAlpha)> {
    let format = source_format(source.format())?;
    let modifier = explicit_modifier(source.modifier())?;
    let mut planes = source.planes();
    let plane = planes
        .next()
        .ok_or_else(|| invalid("renderer source has no memory plane"))?;
    if planes.next().is_some() {
        return Err(unsupported(
            "renderer source has more than one memory plane",
        ));
    }
    let stat = nix::sys::stat::fstat(plane.as_fd().as_raw_fd())?;
    let allocation_size = u64::try_from(stat.st_size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| invalid("renderer source has no addressable DMA-BUF storage"))?;
    let layout = ImageLayout {
        format: format.packed,
        width: source.extent().width().try_into().map_err(invalid)?,
        height: source.extent().height().try_into().map_err(invalid)?,
        modifier,
        offset: u64::from(plane.offset()),
        pitch: u64::from(plane.pitch().get()),
        allocation_size,
    };
    let fd = plane.as_fd().try_clone_to_owned()?;
    let acquire_fence = acquire_fence
        .map(|fence| SyncFile::from_fd(fence.try_clone_to_owned()?))
        .transpose()?;
    // The kernel-issued job retains source-read authority and reports the
    // framebuffer's actual layout. Vulkan validates device compatibility, and
    // the captured acquire fence remains owned by the imported image.
    let image = match acquire_fence {
        Some(acquire_fence) => {
            // SAFETY: The retained job, checked layout and captured acquire
            // fence establish the external-source contract for this import.
            unsafe { device.import_source(fd, layout, acquire_fence) }
        }
        None => {
            // SAFETY: A missing acquire fence from a successfully validated
            // source job means its captured producer work completed.
            unsafe { device.import_ready_source(fd, layout) }
        }
    }?;
    Ok((image, format.alpha))
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceFormat {
    packed: PackedFormat,
    alpha: SourceAlpha,
}

fn source_format(fourcc: u32) -> io::Result<SourceFormat> {
    let (packed, alpha) = match fourcc {
        DRM_FORMAT_XRGB8888 => (PackedFormat::Bgra8, SourceAlpha::Opaque),
        DRM_FORMAT_ARGB8888 => (PackedFormat::Bgra8, SourceAlpha::Channel),
        DRM_FORMAT_XBGR8888 => (PackedFormat::Rgba8, SourceAlpha::Opaque),
        DRM_FORMAT_ABGR8888 => (PackedFormat::Rgba8, SourceAlpha::Channel),
        DRM_FORMAT_XRGB2101010 => (PackedFormat::Bgr10A2, SourceAlpha::Opaque),
        DRM_FORMAT_ARGB2101010 => (PackedFormat::Bgr10A2, SourceAlpha::Channel),
        DRM_FORMAT_XBGR2101010 => (PackedFormat::Rgb10A2, SourceAlpha::Opaque),
        DRM_FORMAT_ABGR2101010 => (PackedFormat::Rgb10A2, SourceAlpha::Channel),
        DRM_FORMAT_RGB565 => (PackedFormat::Rgb565, SourceAlpha::Opaque),
        _ => Err(unsupported(
            "renderer source is not a supported packed RGB format",
        ))?,
    };
    Ok(SourceFormat { packed, alpha })
}

pub(crate) fn packed_format(fourcc: u32) -> io::Result<PackedFormat> {
    source_format(fourcc).map(|format| format.packed)
}

/// Require the exact modifier promised by the selected renderer constraints.
pub(crate) fn explicit_modifier(modifier: FormatModifier) -> io::Result<u64> {
    match modifier {
        FormatModifier::Explicit(modifier) => Ok(modifier),
        FormatModifier::Unspecified => Err(invalid("renderer source has an implicit layout")),
    }
}

#[cfg(test)]
pub(crate) fn source_alpha(fourcc: u32, packed: PackedFormat) -> io::Result<SourceAlpha> {
    let format = source_format(fourcc)?;
    if format.packed != packed {
        return Err(invalid(
            "DRM format does not match the imported source layout",
        ));
    }
    Ok(format.alpha)
}

fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_formats_keep_channel_order_and_alpha_meaning() {
        for (fourcc, packed, alpha) in [
            (
                DRM_FORMAT_XRGB8888,
                PackedFormat::Bgra8,
                SourceAlpha::Opaque,
            ),
            (
                DRM_FORMAT_ARGB8888,
                PackedFormat::Bgra8,
                SourceAlpha::Channel,
            ),
            (
                DRM_FORMAT_XBGR8888,
                PackedFormat::Rgba8,
                SourceAlpha::Opaque,
            ),
            (
                DRM_FORMAT_ABGR8888,
                PackedFormat::Rgba8,
                SourceAlpha::Channel,
            ),
            (
                DRM_FORMAT_XRGB2101010,
                PackedFormat::Bgr10A2,
                SourceAlpha::Opaque,
            ),
            (
                DRM_FORMAT_ARGB2101010,
                PackedFormat::Bgr10A2,
                SourceAlpha::Channel,
            ),
            (
                DRM_FORMAT_XBGR2101010,
                PackedFormat::Rgb10A2,
                SourceAlpha::Opaque,
            ),
            (
                DRM_FORMAT_ABGR2101010,
                PackedFormat::Rgb10A2,
                SourceAlpha::Channel,
            ),
            (DRM_FORMAT_RGB565, PackedFormat::Rgb565, SourceAlpha::Opaque),
        ] {
            let format = source_format(fourcc).unwrap();
            assert_eq!(format.packed, packed);
            assert_eq!(format.alpha, alpha);
        }
    }

    #[test]
    fn source_formats_do_not_infer_alpha_yuv_or_endian_support() {
        for fourcc in [
            u32::from_le_bytes(*b"NV12"),
            u32::from_le_bytes(*b"BG16"),
            DRM_FORMAT_XRGB8888 | (1 << 31),
            DRM_FORMAT_XBGR8888 | (1 << 31),
            DRM_FORMAT_XRGB2101010 | (1 << 31),
            DRM_FORMAT_XBGR2101010 | (1 << 31),
            DRM_FORMAT_RGB565 | (1 << 31),
            0,
        ] {
            assert_eq!(
                source_format(fourcc).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
    }

    #[test]
    fn alpha_semantics_require_the_imported_packed_layout() {
        assert_eq!(
            source_alpha(DRM_FORMAT_XRGB8888, PackedFormat::Bgra8).unwrap(),
            SourceAlpha::Opaque
        );
        assert_eq!(
            source_alpha(DRM_FORMAT_ARGB8888, PackedFormat::Bgra8).unwrap(),
            SourceAlpha::Channel
        );
        assert_eq!(
            source_alpha(DRM_FORMAT_ARGB8888, PackedFormat::Rgba8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn source_import_requires_the_selected_explicit_layout() {
        assert_eq!(
            explicit_modifier(FormatModifier::Unspecified)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(explicit_modifier(FormatModifier::Explicit(9)).unwrap(), 9);
    }
}
