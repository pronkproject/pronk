//! Claimed source ownership and terminal release through an active renderer.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use castkms_sys::{
    drm_ioctl_castkms_renderer_dequeue_source, drm_ioctl_castkms_renderer_release_source,
    DrmCastkmsRendererDequeueSource, DrmCastkmsRendererReleaseSource, DrmCastkmsRendererSource,
    DrmCastkmsRendererSourcePlane, DRM_FORMAT_MOD_INVALID, RENDERER_MAX_PLANES,
    RENDERER_RELEASE_CPU_DONE, RENDERER_RELEASE_NO_ACCESS, RENDERER_RELEASE_SUBMITTED,
};
use drm_display_executor::scene::geometry::{Extent, SourceRect};
use nix::fcntl::{fcntl, FcntlArg};

use super::ActiveRenderer;

/// Whether an image layout carries an explicit DRM format modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatModifier {
    Unspecified,
    Explicit(u64),
}

/// One owned memory plane in a claimed source image.
#[derive(Debug)]
pub struct SourcePlane {
    dma_buf: OwnedFd,
    pitch: NonZeroU32,
    offset: u32,
}

impl SourcePlane {
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

/// Source storage and layout retained until its source job is released.
#[derive(Debug)]
pub struct SourceImage {
    format: u32,
    modifier: FormatModifier,
    extent: Extent,
    planes: [Option<SourcePlane>; RENDERER_MAX_PLANES],
    plane_count: usize,
}

impl SourceImage {
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

/// Validated crop, destination dimensions and complete output dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceGeometry {
    source: SourceRect,
    destination: Extent,
    output: Extent,
}

impl SourceGeometry {
    pub fn source(self) -> SourceRect {
        self.source
    }

    pub fn destination(self) -> Extent {
        self.destination
    }

    pub fn output(self) -> Extent {
        self.output
    }
}

/// One claimed scene source that requires an explicit terminal release.
#[must_use = "release the source job after access ends or close the renderer endpoint"]
#[derive(Debug)]
pub struct SourceJob<'job, 'renderer, F: AsFd> {
    renderer: &'job mut ActiveRenderer<'renderer, F>,
    id: NonZeroU64,
    content_serial: NonZeroU64,
    image: SourceImage,
    geometry: SourceGeometry,
    producer: Option<OwnedFd>,
}

impl<F: AsFd> SourceJob<'_, '_, F> {
    pub fn content_serial(&self) -> NonZeroU64 {
        self.content_serial
    }

    pub fn image(&self) -> &SourceImage {
        &self.image
    }

    pub fn geometry(&self) -> SourceGeometry {
        self.geometry
    }

    /// Return the producer sync file that must complete before source access.
    pub fn producer_completion(&self) -> Option<BorrowedFd<'_>> {
        self.producer.as_ref().map(AsFd::as_fd)
    }

    /// Promise that no source pixels were accessed.
    pub fn release_without_access(self) -> Result<(), SourceReleaseError<Self>> {
        self.release(RENDERER_RELEASE_NO_ACCESS, None)
    }

    /// Promise that all synchronous CPU source access has ended.
    pub fn release_cpu(self) -> Result<(), SourceReleaseError<Self>> {
        self.release(RENDERER_RELEASE_CPU_DONE, None)
    }

    /// Transfer completion for every submitted source read.
    ///
    /// `None` means the native work was already complete when its sync file was
    /// exported. It does not mean that no source access occurred.
    pub fn release_submitted(
        self,
        completion: Option<BorrowedFd<'_>>,
    ) -> Result<(), SourceReleaseError<Self>> {
        self.release(RENDERER_RELEASE_SUBMITTED, completion)
    }

    fn release(
        self,
        kind: u32,
        completion: Option<BorrowedFd<'_>>,
    ) -> Result<(), SourceReleaseError<Self>> {
        if let Err(error) = release_source(self.renderer.as_fd(), self.id, kind, completion) {
            return Err(SourceReleaseError {
                job: Box::new(self),
                error,
            });
        }
        Ok(())
    }
}

/// A failed terminal release that retains the source job for retry.
#[derive(Debug)]
pub struct SourceReleaseError<J> {
    job: Box<J>,
    error: io::Error,
}

impl<J> SourceReleaseError<J> {
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

struct SourceDescription {
    id: NonZeroU64,
    content_serial: NonZeroU64,
    image: SourceImage,
    geometry: SourceGeometry,
    producer: Option<OwnedFd>,
}

struct ReturnedFds {
    producer: Option<OwnedFd>,
    planes: [Option<OwnedFd>; RENDERER_MAX_PLANES],
    invalid: bool,
}

impl<'renderer, F: AsFd> ActiveRenderer<'renderer, F> {
    /// Claim the next changed source from the active scene.
    ///
    /// `None` means no changed source is available. The exclusive borrow
    /// prevents safe code from representing a second outstanding source job
    /// through the same endpoint.
    ///
    /// ```compile_fail
    /// use castkms_renderer::ActiveRenderer;
    /// use std::fmt::Debug;
    /// use std::os::fd::AsFd;
    ///
    /// fn claim_twice<F: AsFd + Debug>(renderer: &mut ActiveRenderer<'_, F>) {
    ///     let first = renderer.try_dequeue_source().unwrap().unwrap();
    ///     let second = renderer.try_dequeue_source().unwrap();
    ///     drop((first, second));
    /// }
    /// ```
    pub fn try_dequeue_source<'job>(
        &'job mut self,
    ) -> io::Result<Option<SourceJob<'job, 'renderer, F>>> {
        let mut result = empty_source_result();
        let request = DrmCastkmsRendererDequeueSource {
            result: (&mut result as *mut DrmCastkmsRendererSource) as u64,
            ..Default::default()
        };
        // SAFETY: The fixed-width input and writable result remain live
        // throughout the synchronous ioctl. Success installs fresh descriptors.
        if let Err(error) =
            unsafe { drm_ioctl_castkms_renderer_dequeue_source(self.as_fd().as_raw_fd(), &request) }
        {
            if error == nix::errno::Errno::ENODATA {
                return Ok(None);
            }
            return Err(error.into());
        }
        let job_id = NonZeroU64::new(result.job_id);
        match validate_source(result) {
            Ok(source) => Ok(Some(SourceJob {
                renderer: self,
                id: source.id,
                content_serial: source.content_serial,
                image: source.image,
                geometry: source.geometry,
                producer: source.producer,
            })),
            Err(error) => {
                if let Some(id) = job_id {
                    let _ = release_source(self.as_fd(), id, RENDERER_RELEASE_NO_ACCESS, None);
                }
                Err(error)
            }
        }
    }
}

fn empty_source_result() -> DrmCastkmsRendererSource {
    DrmCastkmsRendererSource {
        producer_fd: -1,
        planes: [DrmCastkmsRendererSourcePlane {
            dma_buf_fd: -1,
            ..Default::default()
        }; RENDERER_MAX_PLANES],
        ..Default::default()
    }
}

fn validate_source(mut result: DrmCastkmsRendererSource) -> io::Result<SourceDescription> {
    let returned = ReturnedFds::take(&mut result);
    if returned.invalid || result.reserved != 0 {
        return Err(invalid_data("CastKMS returned invalid source descriptors"));
    }
    if returned
        .producer
        .as_ref()
        .is_some_and(|fd| !has_close_on_exec(fd))
    {
        return Err(invalid_data(
            "CastKMS returned a producer descriptor without close-on-exec",
        ));
    }
    let id = NonZeroU64::new(result.job_id)
        .ok_or_else(|| invalid_data("CastKMS returned a zero source job ID"))?;
    let content_serial = NonZeroU64::new(result.content_serial)
        .ok_or_else(|| invalid_data("CastKMS returned a zero content serial"))?;
    let extent = Extent::new(result.width, result.height)
        .map_err(|_| invalid_data("CastKMS returned empty source dimensions"))?;
    let plane_count = usize::try_from(result.plane_count)
        .ok()
        .filter(|count| (1..=RENDERER_MAX_PLANES).contains(count))
        .ok_or_else(|| invalid_data("CastKMS returned an invalid source plane count"))?;

    let mut planes: [Option<SourcePlane>; RENDERER_MAX_PLANES] = std::array::from_fn(|_| None);
    for (index, ((metadata, fd), destination)) in result
        .planes
        .into_iter()
        .zip(returned.planes)
        .zip(planes.iter_mut())
        .enumerate()
    {
        if index < plane_count {
            if metadata.reserved != 0 {
                return Err(invalid_data("CastKMS returned reserved source metadata"));
            }
            let dma_buf =
                fd.ok_or_else(|| invalid_data("CastKMS omitted a source plane descriptor"))?;
            if !has_close_on_exec(&dma_buf) {
                return Err(invalid_data(
                    "CastKMS returned a source descriptor without close-on-exec",
                ));
            }
            *destination = Some(SourcePlane {
                dma_buf,
                pitch: NonZeroU32::new(metadata.pitch)
                    .ok_or_else(|| invalid_data("CastKMS returned a zero source pitch"))?,
                offset: metadata.offset,
            });
        } else if fd.is_some()
            || metadata.pitch != 0
            || metadata.offset != 0
            || metadata.reserved != 0
        {
            return Err(invalid_data("CastKMS initialized an unused source plane"));
        }
    }

    let source = SourceRect::from_fixed_16_16(extent, result.source)
        .map_err(|_| invalid_data("CastKMS returned invalid source coordinates"))?;
    let destination = Extent::new(result.destination[0], result.destination[1])
        .map_err(|_| invalid_data("CastKMS returned empty destination dimensions"))?;
    let output = Extent::new(result.output[0], result.output[1])
        .map_err(|_| invalid_data("CastKMS returned empty output dimensions"))?;
    if destination.width() > output.width() || destination.height() > output.height() {
        return Err(invalid_data(
            "CastKMS returned destination dimensions outside the output",
        ));
    }

    Ok(SourceDescription {
        id,
        content_serial,
        image: SourceImage {
            format: result.format,
            modifier: if result.modifier == DRM_FORMAT_MOD_INVALID {
                FormatModifier::Unspecified
            } else {
                FormatModifier::Explicit(result.modifier)
            },
            extent,
            planes,
            plane_count,
        },
        geometry: SourceGeometry {
            source,
            destination,
            output,
        },
        producer: returned.producer,
    })
}

impl ReturnedFds {
    fn take(result: &mut DrmCastkmsRendererSource) -> Self {
        let mut seen = [-1; RENDERER_MAX_PLANES + 1];
        let mut seen_count = 0;
        let mut invalid = false;
        let producer = take_returned_fd(
            &mut result.producer_fd,
            &mut seen,
            &mut seen_count,
            &mut invalid,
        );
        let planes = std::array::from_fn(|index| {
            take_returned_fd(
                &mut result.planes[index].dma_buf_fd,
                &mut seen,
                &mut seen_count,
                &mut invalid,
            )
        });
        Self {
            producer,
            planes,
            invalid,
        }
    }
}

fn take_returned_fd(
    slot: &mut i32,
    seen: &mut [i32; RENDERER_MAX_PLANES + 1],
    seen_count: &mut usize,
    invalid: &mut bool,
) -> Option<OwnedFd> {
    let raw = std::mem::replace(slot, -1);
    if raw == -1 {
        return None;
    }
    if raw < -1 || seen[..*seen_count].contains(&raw) {
        *invalid = true;
        return None;
    }
    seen[*seen_count] = raw;
    *seen_count += 1;
    // SAFETY: A successful dequeue installs each nonnegative descriptor once.
    // Duplicate values are rejected before another owner is constructed.
    Some(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn has_close_on_exec(fd: &OwnedFd) -> bool {
    fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD).is_ok_and(|flags| flags & nix::libc::FD_CLOEXEC != 0)
}

fn release_source(
    fd: BorrowedFd<'_>,
    id: NonZeroU64,
    kind: u32,
    completion: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let request = DrmCastkmsRendererReleaseSource {
        job_id: id.get(),
        completion_fd: completion.map_or(-1, |fd| fd.as_raw_fd()),
        kind,
        ..Default::default()
    };
    // SAFETY: The initialized request and optional borrowed sync file remain
    // live throughout the synchronous ioctl.
    unsafe { drm_ioctl_castkms_renderer_release_source(fd.as_raw_fd(), &request) }?;
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    use castkms_sys::DRM_FORMAT_XRGB8888;

    use crate::{
        Description, OutputConfiguration, Profile, Renderer, SubmittedCandidate, TakeoverCandidate,
    };

    fn configuration() -> OutputConfiguration {
        OutputConfiguration {
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            refresh_millihz: NonZeroU32::new(60_000).unwrap(),
            mode_flags: 0,
        }
    }

    fn active_renderer(
        renderer: &mut Renderer<std::fs::File>,
    ) -> ActiveRenderer<'_, std::fs::File> {
        ActiveRenderer {
            submitted: SubmittedCandidate {
                candidate: TakeoverCandidate {
                    renderer,
                    id: NonZeroU64::new(9).unwrap(),
                    profile: Profile::HostV1,
                    execution: Description {
                        profile: Profile::HostV1,
                        generation: NonZeroU64::new(7).unwrap(),
                    },
                    configuration: configuration(),
                    active: true,
                },
            },
            description: Description {
                profile: Profile::GpuV1,
                generation: NonZeroU64::new(8).unwrap(),
            },
        }
    }

    fn descriptor() -> i32 {
        std::fs::File::open("/dev/null").unwrap().into_raw_fd()
    }

    fn source_result() -> DrmCastkmsRendererSource {
        let mut result = empty_source_result();
        result.job_id = 13;
        result.content_serial = 14;
        result.modifier = DRM_FORMAT_MOD_INVALID;
        result.format = DRM_FORMAT_XRGB8888;
        result.width = 1920;
        result.height = 1080;
        result.plane_count = 1;
        result.source = [0, 0, 1920 << 16, 1080 << 16];
        result.destination = [1920, 1080];
        result.output = [1920, 1080];
        result.planes[0] = DrmCastkmsRendererSourcePlane {
            dma_buf_fd: descriptor(),
            pitch: 7680,
            ..Default::default()
        };
        result
    }

    #[test]
    fn source_validation_adopts_descriptors_and_geometry() {
        let mut result = source_result();
        result.producer_fd = descriptor();
        let source = validate_source(result).unwrap();
        assert_eq!(source.id.get(), 13);
        assert_eq!(source.content_serial.get(), 14);
        assert_eq!(source.image.format(), DRM_FORMAT_XRGB8888);
        assert_eq!(source.image.modifier(), FormatModifier::Unspecified);
        assert_eq!(source.image.extent().width(), 1920);
        assert_eq!(source.image.planes().len(), 1);
        assert_eq!(source.image.planes().next().unwrap().pitch().get(), 7680);
        assert_eq!(source.geometry.source().extent().width(), 1920);
        assert_eq!(source.geometry.destination().height(), 1080);
        assert!(source.producer.is_some());
    }

    #[test]
    fn source_validation_closes_descriptors_from_malformed_results() {
        let mut result = source_result();
        let first = result.planes[0].dma_buf_fd;
        let second = descriptor();
        result.planes[1].dma_buf_fd = second;
        assert!(validate_source(result).is_err());
        assert_eq!(
            fcntl(first, FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );
        assert_eq!(
            fcntl(second, FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );

        let mut result = source_result();
        let duplicate = result.planes[0].dma_buf_fd;
        result.producer_fd = duplicate;
        assert!(validate_source(result).is_err());
        assert_eq!(
            fcntl(duplicate, FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );
    }

    #[test]
    fn failed_source_release_retains_retry_ownership() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        let mut active = active_renderer(&mut renderer);
        let source = validate_source(source_result()).unwrap();
        let job = SourceJob {
            renderer: &mut active,
            id: source.id,
            content_serial: source.content_serial,
            image: source.image,
            geometry: source.geometry,
            producer: source.producer,
        };
        let error = job.release_without_access().unwrap_err();
        assert_eq!(error.error().raw_os_error(), Some(nix::libc::ENOTTY));
        assert_eq!(error.into_job().content_serial().get(), 14);
    }

    #[test]
    fn failed_cpu_release_retains_retry_ownership() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        let mut active = active_renderer(&mut renderer);
        let source = validate_source(source_result()).unwrap();
        let job = SourceJob {
            renderer: &mut active,
            id: source.id,
            content_serial: source.content_serial,
            image: source.image,
            geometry: source.geometry,
            producer: source.producer,
        };
        let error = job.release_cpu().unwrap_err();
        assert_eq!(error.error().raw_os_error(), Some(nix::libc::ENOTTY));
        let (job, error) = error.into_parts();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ENOTTY));
        assert_eq!(job.content_serial().get(), 14);
    }

    #[test]
    fn failed_submitted_release_retains_job_and_completion_owner() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        let mut active = active_renderer(&mut renderer);
        let source = validate_source(source_result()).unwrap();
        let job = SourceJob {
            renderer: &mut active,
            id: source.id,
            content_serial: source.content_serial,
            image: source.image,
            geometry: source.geometry,
            producer: source.producer,
        };
        let completion = std::fs::File::open("/dev/null").unwrap();
        let error = job.release_submitted(Some(completion.as_fd())).unwrap_err();
        assert_eq!(error.error().raw_os_error(), Some(nix::libc::ENOTTY));
        assert!(fcntl(completion.as_raw_fd(), FcntlArg::F_GETFD).is_ok());
        assert_eq!(error.into_job().content_serial().get(), 14);
    }

    #[test]
    fn completed_submission_uses_the_native_sync_file_sentinel() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        let mut active = active_renderer(&mut renderer);
        let source = validate_source(source_result()).unwrap();
        let job = SourceJob {
            renderer: &mut active,
            id: source.id,
            content_serial: source.content_serial,
            image: source.image,
            geometry: source.geometry,
            producer: source.producer,
        };
        let error = job.release_submitted(None).unwrap_err();
        assert_eq!(error.error().raw_os_error(), Some(nix::libc::ENOTTY));
        assert_eq!(error.into_job().content_serial().get(), 14);
    }

    #[test]
    fn ordinary_files_reject_source_dequeue() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        let mut active = active_renderer(&mut renderer);
        assert_eq!(
            active.try_dequeue_source().unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
    }
}
