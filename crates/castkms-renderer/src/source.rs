//! Source-image ownership and terminal release for complete renderer scenes.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use castkms_sys::{
    drm_ioctl_castkms_renderer_release_job, DrmCastkmsRendererReleaseJob,
    RENDERER_MAX_MEMORY_PLANES,
};
use drm_display_executor::scene::geometry::Extent;
use nix::fcntl::{fcntl, FcntlArg};

/// Whether an image layout carries an explicit DRM format modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatModifier {
    Unspecified,
    Explicit(u64),
}

/// One owned memory plane in a claimed scene image.
#[derive(Debug)]
pub struct SourcePlane {
    dma_buf: OwnedFd,
    pitch: NonZeroU32,
    offset: u32,
}

impl SourcePlane {
    pub(super) fn from_parts(dma_buf: OwnedFd, pitch: NonZeroU32, offset: u32) -> Self {
        Self {
            dma_buf,
            pitch,
            offset,
        }
    }

    pub fn pitch(&self) -> NonZeroU32 {
        self.pitch
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }
}

impl AsFd for SourcePlane {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dma_buf.as_fd()
    }
}

/// Source storage and layout retained until its scene job is released.
#[derive(Debug)]
pub struct SourceImage {
    format: u32,
    modifier: FormatModifier,
    extent: Extent,
    planes: [Option<SourcePlane>; RENDERER_MAX_MEMORY_PLANES],
    plane_count: usize,
}

impl SourceImage {
    pub(super) fn from_parts(
        format: u32,
        modifier: FormatModifier,
        extent: Extent,
        planes: [Option<SourcePlane>; RENDERER_MAX_MEMORY_PLANES],
        plane_count: usize,
    ) -> Self {
        debug_assert!((1..=RENDERER_MAX_MEMORY_PLANES).contains(&plane_count));
        debug_assert!(planes[..plane_count].iter().all(Option::is_some));
        debug_assert!(planes[plane_count..].iter().all(Option::is_none));
        Self {
            format,
            modifier,
            extent,
            planes,
            plane_count,
        }
    }

    pub fn format(&self) -> u32 {
        self.format
    }

    pub fn modifier(&self) -> FormatModifier {
        self.modifier
    }

    pub fn extent(&self) -> Extent {
        self.extent
    }

    pub fn planes(&self) -> impl ExactSizeIterator<Item = &SourcePlane> {
        self.planes[..self.plane_count]
            .iter()
            .map(|plane| plane.as_ref().expect("validated source plane"))
    }
}

/// A failed terminal release that retains the scene job for retry.
#[derive(Debug)]
pub struct SourceReleaseError<J> {
    job: Box<J>,
    error: io::Error,
}

impl<J> SourceReleaseError<J> {
    pub(super) fn new(job: J, error: io::Error) -> Self {
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

pub(super) fn has_close_on_exec(fd: &OwnedFd) -> bool {
    fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD)
        .map(|flags| flags & nix::libc::FD_CLOEXEC != 0)
        .unwrap_or(false)
}

pub(super) fn release_job(
    fd: BorrowedFd<'_>,
    id: NonZeroU64,
    kind: u32,
    completion: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let request = DrmCastkmsRendererReleaseJob {
        job_id: id.get(),
        kind,
        release_fence_fd: completion.map_or(-1, |fd| fd.as_raw_fd()),
        ..Default::default()
    };
    // SAFETY: The fixed-width request remains live throughout the synchronous
    // ioctl, and any completion descriptor is borrowed for that duration.
    unsafe { drm_ioctl_castkms_renderer_release_job(fd.as_raw_fd(), &request) }?;
    Ok(())
}
