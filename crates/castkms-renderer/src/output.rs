//! Recipient-output ownership after compositor sources have retired.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use castkms_sys::{
    drm_ioctl_castkms_renderer_acquire_output, drm_ioctl_castkms_renderer_release_output,
    DrmCastkmsRendererAcquireOutput, DrmCastkmsRendererOutput, DrmCastkmsRendererReleaseOutput,
};
use drm_display_executor::scene::geometry::Extent;

use crate::source::has_close_on_exec;
use crate::{PublishedRenderer, RegisteredImage};

/// An independently serialized channel for completed private images.
///
/// The channel owns a duplicate of the renderer endpoint descriptor. It can
/// therefore wait for recipient destinations without borrowing the scene
/// channel or retaining compositor sources.
#[derive(Debug)]
pub struct OutputChannel {
    fd: OwnedFd,
    image_scope: Arc<()>,
}

impl AsFd for OutputChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// One writable recipient image claimed by CastKMS.
#[derive(Debug)]
pub struct RecipientImage {
    dma_buf: OwnedFd,
    format: u32,
    modifier: u64,
    extent: Extent,
    pitch: NonZeroU32,
    offset: u64,
    allocation_size: u64,
}

impl RecipientImage {
    pub fn format(&self) -> u32 {
        self.format
    }

    pub fn modifier(&self) -> u64 {
        self.modifier
    }

    pub fn extent(&self) -> Extent {
        self.extent
    }

    pub fn pitch(&self) -> NonZeroU32 {
        self.pitch
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn allocation_size(&self) -> u64 {
        self.allocation_size
    }
}

impl AsFd for RecipientImage {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dma_buf.as_fd()
    }
}

/// One private-image-to-recipient claim requiring a terminal release.
#[must_use = "release the output job after every private read and recipient write ends"]
#[derive(Debug)]
pub struct OutputJob<'job> {
    channel: &'job mut OutputChannel,
    id: NonZeroU64,
    image: RecipientImage,
}

impl OutputJob<'_> {
    pub fn destination(&self) -> &RecipientImage {
        &self.image
    }

    pub fn release_without_access(self) -> Result<(), OutputReleaseError<Self>> {
        self.release(castkms_sys::RENDERER_RELEASE_NO_ACCESS, None)
    }

    /// Promise that all synchronous CPU access to both images has ended.
    pub fn release_cpu(self) -> Result<(), OutputReleaseError<Self>> {
        self.release(castkms_sys::RENDERER_RELEASE_CPU_DONE, None)
    }

    pub fn release_submitted(
        self,
        completion: BorrowedFd<'_>,
    ) -> Result<(), OutputReleaseError<Self>> {
        self.release(castkms_sys::RENDERER_RELEASE_SUBMITTED, Some(completion))
    }

    fn release(
        self,
        kind: u32,
        completion: Option<BorrowedFd<'_>>,
    ) -> Result<(), OutputReleaseError<Self>> {
        if let Err(error) = release_output(self.channel.as_fd(), self.id, kind, completion) {
            return Err(OutputReleaseError::new(self, error));
        }
        Ok(())
    }
}

/// A failed terminal release that retains the output job for retry.
#[derive(Debug)]
pub struct OutputReleaseError<J> {
    job: Box<J>,
    error: io::Error,
}

impl<J> OutputReleaseError<J> {
    fn new(job: J, error: io::Error) -> Self {
        Self {
            job: Box::new(job),
            error,
        }
    }

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

impl<J> std::fmt::Display for OutputReleaseError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl<J: std::fmt::Debug> std::error::Error for OutputReleaseError<J> {}

impl<F: AsFd> PublishedRenderer<F> {
    /// Open the endpoint's one independently owned recipient-output channel.
    pub fn open_output_channel(&mut self) -> io::Result<OutputChannel> {
        if self.configuration.endpoint.output_channel_issued {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "renderer output channel is already open",
            ));
        }
        let fd = self.as_fd().try_clone_to_owned()?;
        self.configuration.endpoint.output_channel_issued = true;
        Ok(OutputChannel {
            fd,
            image_scope: Arc::clone(&self.configuration.endpoint.image_scope),
        })
    }
}

impl OutputChannel {
    /// Claim one ready recipient destination for a completed private image.
    ///
    /// `None` means no compatible recipient is ready. The exclusive borrow
    /// permits only one output claim on this channel until terminal release.
    ///
    /// ```compile_fail
    /// use castkms_renderer::{OutputChannel, RegisteredImage};
    ///
    /// fn claim_twice(channel: &mut OutputChannel, image: &RegisteredImage) {
    ///     let first = channel.try_acquire(image).unwrap().unwrap();
    ///     let second = channel.try_acquire(image).unwrap();
    ///     drop((first, second));
    /// }
    /// ```
    pub fn try_acquire<'job>(
        &'job mut self,
        image: &RegisteredImage,
    ) -> io::Result<Option<OutputJob<'job>>> {
        if !image.belongs_to(&self.image_scope) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private image belongs to another renderer",
            ));
        }
        let mut result = DrmCastkmsRendererOutput {
            dma_buf_fd: -1,
            ..Default::default()
        };
        let request = DrmCastkmsRendererAcquireOutput {
            result: (&mut result as *mut DrmCastkmsRendererOutput) as u64,
            image_id: image.id().get(),
            ..Default::default()
        };
        // SAFETY: The initialized request and writable result remain live
        // throughout the synchronous ioctl. Success installs one fresh
        // close-on-exec descriptor in the result record.
        if let Err(error) =
            unsafe { drm_ioctl_castkms_renderer_acquire_output(self.as_fd().as_raw_fd(), &request) }
        {
            if crate::acquisition_is_idle(error) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let job_id = NonZeroU64::new(result.job_id);
        let decoded = decode_output(result, image.id());
        let (id, destination) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                if let Some(id) = job_id {
                    let _ = release_output(
                        self.as_fd(),
                        id,
                        castkms_sys::RENDERER_RELEASE_NO_ACCESS,
                        None,
                    );
                }
                return Err(error);
            }
        };
        Ok(Some(OutputJob {
            channel: self,
            id,
            image: destination,
        }))
    }
}

fn decode_output(
    raw: DrmCastkmsRendererOutput,
    expected_image: NonZeroU64,
) -> io::Result<(NonZeroU64, RecipientImage)> {
    let dma_buf = if raw.dma_buf_fd >= 0 {
        // SAFETY: A successful acquisition installs one fresh descriptor. The
        // integer is adopted exactly once before validating the other fields.
        Some(unsafe { OwnedFd::from_raw_fd(raw.dma_buf_fd) })
    } else {
        None
    };
    let id = NonZeroU64::new(raw.job_id)
        .ok_or_else(|| invalid("CastKMS returned a zero output job ID"))?;
    if raw.image_id != expected_image.get() {
        return Err(invalid("CastKMS returned output for another private image"));
    }
    if raw.memory_plane_count != 1
        || raw.format == 0
        || raw.modifier == castkms_sys::DRM_FORMAT_MOD_INVALID
        || raw.reserved != [0; 2]
    {
        return Err(invalid("CastKMS returned invalid output metadata"));
    }
    let extent = Extent::new(raw.width, raw.height)
        .map_err(|_| invalid("CastKMS returned empty output dimensions"))?;
    let pitch = NonZeroU32::new(raw.pitch)
        .ok_or_else(|| invalid("CastKMS returned a zero output pitch"))?;
    let dma_buf = dma_buf.ok_or_else(|| invalid("CastKMS omitted the output descriptor"))?;
    if !has_close_on_exec(&dma_buf) {
        return Err(invalid(
            "CastKMS returned an output descriptor without close-on-exec",
        ));
    }
    let allocation_size = nix::sys::stat::fstat(dma_buf.as_raw_fd())?
        .st_size
        .try_into()
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| invalid("CastKMS returned output without addressable storage"))?;
    if raw.offset >= allocation_size {
        return Err(invalid("CastKMS returned output outside its storage"));
    }
    if raw.modifier == castkms_sys::DRM_FORMAT_MOD_LINEAR {
        let end = u64::from(pitch.get())
            .checked_mul(u64::from(extent.height()))
            .and_then(|span| raw.offset.checked_add(span))
            .ok_or_else(|| invalid("CastKMS returned an overflowing output layout"))?;
        if end > allocation_size {
            return Err(invalid("CastKMS returned output outside its storage"));
        }
    }
    Ok((
        id,
        RecipientImage {
            dma_buf,
            format: raw.format,
            modifier: raw.modifier,
            extent,
            pitch,
            offset: raw.offset,
            allocation_size,
        },
    ))
}

fn release_output(
    fd: BorrowedFd<'_>,
    id: NonZeroU64,
    kind: u32,
    completion: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let request = DrmCastkmsRendererReleaseOutput {
        job_id: id.get(),
        release_fence_fd: completion.map_or(-1, |fd| fd.as_raw_fd()),
        kind,
        ..Default::default()
    };
    // SAFETY: The fixed-width request remains live throughout the synchronous
    // ioctl, and any completion descriptor is borrowed for that duration.
    unsafe { drm_ioctl_castkms_renderer_release_output(fd.as_raw_fd(), &request) }?;
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;

    use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
    use nix::unistd::ftruncate;

    use super::*;

    fn raw_output(fd: OwnedFd) -> DrmCastkmsRendererOutput {
        DrmCastkmsRendererOutput {
            job_id: 7,
            image_id: 3,
            width: 64,
            height: 32,
            format: castkms_sys::DRM_FORMAT_XRGB8888,
            memory_plane_count: 1,
            modifier: castkms_sys::DRM_FORMAT_MOD_LINEAR,
            dma_buf_fd: fd.into_raw_fd(),
            pitch: 256,
            ..Default::default()
        }
    }

    fn storage(bytes: u64) -> OwnedFd {
        let fd = memfd_create(c"castkms-output", MemFdCreateFlag::MFD_CLOEXEC).unwrap();
        ftruncate(&fd, i64::try_from(bytes).unwrap()).unwrap();
        fd
    }

    #[test]
    fn output_record_retains_its_exact_layout() {
        let (id, image) =
            decode_output(raw_output(storage(8192)), NonZeroU64::new(3).unwrap()).unwrap();
        assert_eq!(id.get(), 7);
        assert_eq!(image.extent(), Extent::new(64, 32).unwrap());
        assert_eq!(image.pitch().get(), 256);
        assert_eq!(image.offset(), 0);
        assert_eq!(image.allocation_size(), 8192);
    }

    #[test]
    fn output_record_rejects_wrong_identity_and_bounds() {
        let (fd, mut peer) = UnixStream::pair().unwrap();
        assert!(decode_output(raw_output(fd.into()), NonZeroU64::new(4).unwrap()).is_err());
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);

        let mut raw = raw_output(storage(8191));
        raw.offset = 1;
        assert!(decode_output(raw, NonZeroU64::new(3).unwrap()).is_err());
    }

    #[test]
    fn tiled_output_is_not_measured_as_linear_rows() {
        let mut raw = raw_output(storage(4096));
        raw.format = castkms_sys::DRM_FORMAT_ARGB8888;
        raw.modifier = 9;
        raw.pitch = 512;
        raw.offset = 64;
        let (_, image) = decode_output(raw, NonZeroU64::new(3).unwrap()).unwrap();
        assert_eq!(image.format(), castkms_sys::DRM_FORMAT_ARGB8888);
        assert_eq!(image.modifier(), 9);
        assert_eq!(image.offset(), 64);

        let mut outside = raw_output(storage(4096));
        outside.modifier = 9;
        outside.offset = 4096;
        assert!(decode_output(outside, NonZeroU64::new(3).unwrap()).is_err());

        let mut invalid_modifier = raw_output(storage(8192));
        invalid_modifier.modifier = castkms_sys::DRM_FORMAT_MOD_INVALID;
        assert!(decode_output(invalid_modifier, NonZeroU64::new(3).unwrap()).is_err());
    }

    #[test]
    fn output_record_requires_close_on_exec() {
        let fd = memfd_create(c"castkms-output", MemFdCreateFlag::empty()).unwrap();
        ftruncate(&fd, 8192).unwrap();
        assert!(decode_output(raw_output(fd), NonZeroU64::new(3).unwrap()).is_err());
    }
}
