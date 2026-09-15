//! Checked adaptation of one claimed CastKMS source into Vulkan ownership.

use std::io;
use std::os::fd::{AsFd, AsRawFd};

use castkms_renderer::{FormatModifier, SourceGeometry, SourceJob, SourceReleaseError};
use castkms_sys::{
    DRM_FORMAT_ABGR2101010, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_RGB565, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XRGB8888,
};
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{Device, ImageLayout, PackedFormat, SourceImage};

/// One claimed source paired with its ordinary Vulkan import.
#[must_use = "release the source without access or submit its native read"]
pub struct ImportedSource<'job, 'renderer, F: AsFd> {
    pub(super) job: SourceJob<'job, 'renderer, F>,
    pub(super) image: SourceImage,
    pub(super) geometry: SourceGeometry,
    pub(super) alpha: SourceAlpha,
}

/// Whether the imported fourth channel contains alpha or ignored padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceAlpha {
    /// The fourth component is padding, or the format has no alpha component.
    Opaque,
    /// The native fourth component contains normalized pixel alpha.
    Channel,
}

impl<'job, 'renderer, F: AsFd> ImportedSource<'job, 'renderer, F> {
    /// Import the source through the selected Vulkan device without reading it.
    pub fn new(
        device: &Device,
        job: SourceJob<'job, 'renderer, F>,
    ) -> Result<Self, ImportError<SourceJob<'job, 'renderer, F>>> {
        match import(device, &job) {
            Ok((image, alpha)) => Ok(Self {
                geometry: job.geometry(),
                job,
                image,
                alpha,
            }),
            Err(error) => Err(ImportError {
                job: Box::new(job),
                error,
            }),
        }
    }

    pub fn geometry(&self) -> SourceGeometry {
        self.geometry
    }

    pub fn alpha(&self) -> SourceAlpha {
        self.alpha
    }

    /// Destroy the unused import before promising that no pixels were accessed.
    pub fn release_without_access(
        self,
    ) -> Result<(), SourceReleaseError<SourceJob<'job, 'renderer, F>>> {
        let Self { job, image, .. } = self;
        drop(image);
        job.release_without_access()
    }
}

/// A failed pre-submission import retaining the source job for no-access release.
#[derive(Debug)]
pub struct ImportError<J> {
    job: Box<J>,
    error: io::Error,
}

impl<J> ImportError<J> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_job(self) -> J {
        *self.job
    }

    pub fn into_parts(self) -> (J, io::Error) {
        (*self.job, self.error)
    }
}

fn import<F: AsFd>(
    device: &Device,
    job: &SourceJob<'_, '_, F>,
) -> io::Result<(SourceImage, SourceAlpha)> {
    let source = job.image();
    let format = source_format(source.format())?;
    let modifier = match source.modifier() {
        FormatModifier::Explicit(modifier) => modifier,
        FormatModifier::Unspecified => {
            return Err(unsupported(
                "renderer source has no explicit format modifier",
            ));
        }
    };
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
    let producer = job
        .producer_completion()
        .map(|producer| SyncFile::from_fd(producer.try_clone_to_owned()?))
        .transpose()?;
    // The kernel-issued job retains source-read authority and reports the
    // framebuffer's actual layout. Vulkan validates device compatibility, and
    // the captured producer record remains owned by the imported image.
    let image = match producer {
        Some(producer) => {
            // SAFETY: The retained job, checked layout and captured producer
            // record establish the external-source contract for this import.
            unsafe { device.import_source(fd, layout, producer) }
        }
        None => {
            // SAFETY: A missing producer descriptor from a successfully
            // validated source job means its captured producer work completed.
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
}
