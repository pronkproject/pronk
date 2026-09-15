//! Checked adaptation of one claimed CastKMS source into Vulkan ownership.

use std::io;
use std::os::fd::{AsFd, AsRawFd};

use castkms_renderer::{FormatModifier, SourceGeometry, SourceJob, SourceReleaseError};
use castkms_sys::{DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888};
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{Device, ImageLayout, PackedFormat, SourceImage};

/// One claimed source paired with its ordinary Vulkan import.
#[must_use = "release the source without access or submit its native read"]
pub struct ImportedSource<'job, 'renderer, F: AsFd> {
    pub(super) job: SourceJob<'job, 'renderer, F>,
    pub(super) image: SourceImage,
    pub(super) geometry: SourceGeometry,
}

impl<'job, 'renderer, F: AsFd> ImportedSource<'job, 'renderer, F> {
    /// Import the source through the selected Vulkan device without reading it.
    pub fn new(
        device: &Device,
        job: SourceJob<'job, 'renderer, F>,
    ) -> Result<Self, ImportError<SourceJob<'job, 'renderer, F>>> {
        match import(device, &job) {
            Ok(image) => Ok(Self {
                geometry: job.geometry(),
                job,
                image,
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

fn import<F: AsFd>(device: &Device, job: &SourceJob<'_, '_, F>) -> io::Result<SourceImage> {
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
        format,
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
    match producer {
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
    }
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn source_format(fourcc: u32) -> io::Result<PackedFormat> {
    match fourcc {
        DRM_FORMAT_XRGB8888 => Ok(PackedFormat::Bgra8),
        DRM_FORMAT_XBGR8888 => Ok(PackedFormat::Rgba8),
        _ => Err(unsupported(
            "renderer source is not a supported opaque packed RGB format",
        )),
    }
}

fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_source_formats_keep_their_native_channel_order() {
        assert_eq!(
            source_format(DRM_FORMAT_XRGB8888).unwrap(),
            PackedFormat::Bgra8
        );
        assert_eq!(
            source_format(DRM_FORMAT_XBGR8888).unwrap(),
            PackedFormat::Rgba8
        );
    }

    #[test]
    fn source_formats_do_not_infer_alpha_yuv_or_endian_support() {
        for fourcc in [
            u32::from_le_bytes(*b"AR24"),
            u32::from_le_bytes(*b"AB24"),
            u32::from_le_bytes(*b"NV12"),
            u32::from_le_bytes(*b"RG16"),
            DRM_FORMAT_XRGB8888 | (1 << 31),
            DRM_FORMAT_XBGR8888 | (1 << 31),
            0,
        ] {
            assert_eq!(
                source_format(fourcc).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
    }
}
