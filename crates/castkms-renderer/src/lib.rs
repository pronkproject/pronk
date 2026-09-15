//! Renderer startup through an anonymous CastKMS capability.
//!
//! A renderer descriptor grants no modesetting or final-image capture access.
//! Its operations reserve one takeover candidate and optionally copy the most
//! recent HOST result into independent, read-only storage. An active renderer
//! can claim one source or complete-scene job whose consuming release records
//! how source access ended.

mod scene;
mod source;

pub use scene::{ColorEncoding, ColorOperation, ColorRange, LayerKind, SceneJob, SceneLayer};
pub use source::{
    FormatModifier, SourceGeometry, SourceImage, SourceJob, SourcePlane, SourceReleaseError,
};

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use castkms_sys::{
    drm_ioctl_castkms_renderer_abort_takeover, drm_ioctl_castkms_renderer_begin_takeover,
    drm_ioctl_castkms_renderer_commit_takeover, drm_ioctl_castkms_renderer_get_snapshot,
    drm_ioctl_castkms_renderer_query, drm_ioctl_castkms_renderer_submit_probe,
    DrmCastkmsRendererAbortTakeover, DrmCastkmsRendererBeginTakeover,
    DrmCastkmsRendererCommitTakeover, DrmCastkmsRendererGetSnapshot, DrmCastkmsRendererQuery,
    DrmCastkmsRendererSnapshot, DrmCastkmsRendererSubmitProbe, DrmCastkmsRendererTakeover,
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, EXECUTION_GPU_V1, EXECUTION_HOST_V1,
    RENDERER_PROBE_PRIVATE, RENDERER_PROBE_STARTUP_IMAGE, RENDERER_VERSION,
};
use nix::fcntl::{fcntl, FcntlArg};

/// Execution implementation active when a renderer observation was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    HostV1,
    GpuV1,
}

impl Profile {
    fn from_uapi(value: u32) -> io::Result<Self> {
        match value {
            EXECUTION_HOST_V1 => Ok(Self::HostV1),
            EXECUTION_GPU_V1 => Ok(Self::GpuV1),
            _ => Err(unsupported("unknown CastKMS execution profile")),
        }
    }
}

/// A non-reserving observation of the renderer endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Description {
    profile: Profile,
    generation: NonZeroU64,
}

impl Description {
    pub fn profile(self) -> Profile {
        self.profile
    }

    pub fn generation(self) -> NonZeroU64 {
        self.generation
    }
}

/// Output configuration retained by a takeover candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputConfiguration {
    width: NonZeroU32,
    height: NonZeroU32,
    refresh_millihz: NonZeroU32,
    mode_flags: u32,
}

impl OutputConfiguration {
    pub fn width(self) -> NonZeroU32 {
        self.width
    }

    pub fn height(self) -> NonZeroU32 {
        self.height
    }

    pub fn refresh_millihz(self) -> NonZeroU32 {
        self.refresh_millihz
    }

    pub fn mode_flags(self) -> u32 {
        self.mode_flags
    }
}

/// Independent immutable image copied while HOST execution remains active.
#[derive(Debug)]
pub struct StartupImage {
    dma_buf: OwnedFd,
    width: NonZeroU32,
    height: NonZeroU32,
    pitch: NonZeroU32,
    content_serial: Option<NonZeroU64>,
}

impl StartupImage {
    pub fn width(&self) -> NonZeroU32 {
        self.width
    }

    pub fn height(&self) -> NonZeroU32 {
        self.height
    }

    pub fn pitch(&self) -> NonZeroU32 {
        self.pitch
    }

    /// Return the historical content identity, or `None` for a blank image.
    pub fn content_serial(&self) -> Option<NonZeroU64> {
        self.content_serial
    }
}

impl AsFd for StartupImage {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dma_buf.as_fd()
    }
}

/// One renderer capability owner.
#[derive(Debug)]
pub struct Renderer<F = OwnedFd> {
    fd: F,
}

impl Renderer {
    /// Adopt and validate an inherited renderer descriptor.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Self::from_owner(fd)
    }
}

impl<F: AsFd> Renderer<F> {
    /// Retain an arbitrary descriptor owner after validating its endpoint.
    pub fn from_owner(fd: F) -> io::Result<Self> {
        let renderer = Self { fd };
        renderer.describe()?;
        Ok(renderer)
    }

    /// Observe the execution generation without reserving a takeover.
    pub fn describe(&self) -> io::Result<Description> {
        let mut query = DrmCastkmsRendererQuery::default();
        // SAFETY: The writable fixed-width response remains live throughout the
        // synchronous ioctl.
        unsafe { drm_ioctl_castkms_renderer_query(self.fd.as_fd().as_raw_fd(), &mut query) }?;
        validate_description(query)
    }

    /// Reserve startup against a previously observed execution generation.
    ///
    /// The exclusive borrow prevents a second candidate from being represented
    /// through the same endpoint until the returned value is aborted or dropped.
    ///
    /// ```compile_fail
    /// use castkms_renderer::{Description, Renderer};
    /// use std::os::fd::AsFd;
    ///
    /// fn reserve_twice<F: AsFd>(renderer: &mut Renderer<F>, state: Description) {
    ///     let first = renderer.begin_takeover(state).unwrap();
    ///     let second = renderer.begin_takeover(state).unwrap();
    ///     drop((first, second));
    /// }
    /// ```
    pub fn begin_takeover(
        &mut self,
        expected: Description,
    ) -> io::Result<TakeoverCandidate<'_, F>> {
        let mut result = DrmCastkmsRendererTakeover::default();
        let request = DrmCastkmsRendererBeginTakeover {
            expected_generation: expected.generation.get(),
            result: (&mut result as *mut DrmCastkmsRendererTakeover) as u64,
            ..Default::default()
        };
        // SAFETY: The fixed-width input and separate writable result remain live
        // throughout the synchronous ioctl.
        unsafe {
            drm_ioctl_castkms_renderer_begin_takeover(self.fd.as_fd().as_raw_fd(), &request)
        }?;
        let description = match validate_candidate(result, expected) {
            Ok(description) => description,
            Err(error) => {
                if let Some(id) = NonZeroU64::new(result.candidate_id) {
                    let _ = abort(self.as_fd(), id);
                }
                return Err(error);
            }
        };
        Ok(TakeoverCandidate {
            renderer: self,
            id: description.id,
            profile: description.profile,
            execution: expected,
            configuration: description.configuration,
            active: true,
        })
    }

    /// Return the complete descriptor owner without changing kernel state.
    pub fn into_owner(self) -> F {
        self.fd
    }
}

impl<F: AsFd> AsFd for Renderer<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

struct CandidateDescription {
    id: NonZeroU64,
    profile: Profile,
    configuration: OutputConfiguration,
}

/// One reserved takeover candidate tied to its originating renderer endpoint.
#[must_use = "retain the candidate for startup or abort it deliberately"]
#[derive(Debug)]
pub struct TakeoverCandidate<'renderer, F: AsFd> {
    renderer: &'renderer mut Renderer<F>,
    id: NonZeroU64,
    profile: Profile,
    execution: Description,
    configuration: OutputConfiguration,
    active: bool,
}

impl<'renderer, F: AsFd> TakeoverCandidate<'renderer, F> {
    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn configuration(&self) -> OutputConfiguration {
        self.configuration
    }

    /// Copy the newest eligible HOST result into independent storage.
    ///
    /// `ENODATA` means no retained HOST image is available. The operation does
    /// not request a new HOST copy. Success pairs the only delivered image with
    /// the candidate; any error aborts the consumed candidate.
    pub fn startup_image(self) -> io::Result<StartupCandidate<'renderer, F>> {
        let mut result = DrmCastkmsRendererSnapshot {
            dma_buf_fd: -1,
            ..Default::default()
        };
        let request = DrmCastkmsRendererGetSnapshot {
            candidate_id: self.id.get(),
            result: (&mut result as *mut DrmCastkmsRendererSnapshot) as u64,
            ..Default::default()
        };
        // SAFETY: The fixed-width input and separate writable result remain live
        // throughout the synchronous ioctl. Success installs one fresh descriptor.
        unsafe {
            drm_ioctl_castkms_renderer_get_snapshot(self.renderer.fd.as_fd().as_raw_fd(), &request)
        }?;
        if result.dma_buf_fd < 0 {
            return Err(invalid_data("CastKMS returned an invalid startup image fd"));
        }
        // SAFETY: A successful snapshot call installs one fresh descriptor for
        // the caller, and no other Rust owner has adopted it.
        let dma_buf = unsafe { OwnedFd::from_raw_fd(result.dma_buf_fd) };
        let image = validate_startup_image(result, dma_buf, self.configuration)?;
        Ok(StartupCandidate {
            candidate: self,
            image,
        })
    }

    /// Submit test work over renderer-owned storage.
    ///
    /// The candidate is consumed so safe Rust cannot submit a second test or
    /// request a startup image afterward. The native completion may remain
    /// pending after the operation returns.
    ///
    /// ```compile_fail
    /// use castkms_renderer::TakeoverCandidate;
    /// use std::os::fd::AsFd;
    ///
    /// fn submit_twice<F: AsFd>(candidate: TakeoverCandidate<'_, F>) {
    ///     let submitted = candidate.submit_private_probe(None).unwrap();
    ///     submitted.submit_private_probe(None);
    /// }
    /// ```
    pub fn submit_private_probe(
        self,
        completion: Option<BorrowedFd<'_>>,
    ) -> io::Result<SubmittedCandidate<'renderer, F>> {
        self.submit_probe(RENDERER_PROBE_PRIVATE, completion)
    }

    fn submit_probe(
        self,
        source: u32,
        completion: Option<BorrowedFd<'_>>,
    ) -> io::Result<SubmittedCandidate<'renderer, F>> {
        let request = DrmCastkmsRendererSubmitProbe {
            candidate_id: self.id.get(),
            completion_fd: completion.map_or(-1, |fd| fd.as_raw_fd()),
            source,
            ..Default::default()
        };
        // SAFETY: The initialized fixed-width request and any borrowed
        // completion descriptor remain live throughout the synchronous ioctl.
        unsafe {
            drm_ioctl_castkms_renderer_submit_probe(self.renderer.fd.as_fd().as_raw_fd(), &request)
        }?;
        Ok(SubmittedCandidate { candidate: self })
    }

    /// Release the candidate without changing the active execution profile.
    pub fn abort(mut self) -> io::Result<()> {
        let result = abort(self.renderer.as_fd(), self.id);
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

/// A candidate paired with the independent startup image copied for it.
#[must_use = "submit startup test work or abort the candidate deliberately"]
#[derive(Debug)]
pub struct StartupCandidate<'renderer, F: AsFd> {
    candidate: TakeoverCandidate<'renderer, F>,
    image: StartupImage,
}

impl<'renderer, F: AsFd> StartupCandidate<'renderer, F> {
    pub fn image(&self) -> &StartupImage {
        &self.image
    }

    /// Submit test work that uploaded the paired startup image.
    pub fn submit_probe(
        self,
        completion: Option<BorrowedFd<'_>>,
    ) -> io::Result<SubmittedCandidate<'renderer, F>> {
        let Self { candidate, image } = self;
        let submitted = candidate.submit_probe(RENDERER_PROBE_STARTUP_IMAGE, completion);
        drop(image);
        submitted
    }

    /// Release the candidate without changing the active execution profile.
    pub fn abort(self) -> io::Result<()> {
        self.candidate.abort()
    }
}

/// A takeover candidate with one native test operation submitted to the kernel.
#[must_use = "retain the submitted candidate for activation or abort it deliberately"]
#[derive(Debug)]
pub struct SubmittedCandidate<'renderer, F: AsFd> {
    candidate: TakeoverCandidate<'renderer, F>,
}

impl<'renderer, F: AsFd> SubmittedCandidate<'renderer, F> {
    pub fn profile(&self) -> Profile {
        self.candidate.profile()
    }

    pub fn configuration(&self) -> OutputConfiguration {
        self.candidate.configuration()
    }

    /// Publish delegated execution after the submitted native work completes.
    ///
    /// Failure returns ownership with the error so a pending completion can be
    /// retried. Success transfers the exclusive renderer borrow into an active
    /// renderer handle.
    pub fn activate(
        mut self,
    ) -> Result<ActiveRenderer<'renderer, F>, ActivationError<'renderer, F>> {
        let generation = match self
            .candidate
            .execution
            .generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
        {
            Some(generation) => generation,
            None => {
                return Err(ActivationError {
                    submitted: self,
                    error: invalid_data("CastKMS execution generation overflowed"),
                });
            }
        };
        let request = DrmCastkmsRendererCommitTakeover {
            candidate_id: self.candidate.id.get(),
            ..Default::default()
        };
        // SAFETY: The initialized fixed-width request remains live throughout
        // the synchronous ioctl.
        if let Err(error) = unsafe {
            drm_ioctl_castkms_renderer_commit_takeover(
                self.candidate.renderer.fd.as_fd().as_raw_fd(),
                &request,
            )
        } {
            return Err(ActivationError {
                submitted: self,
                error: error.into(),
            });
        }
        self.candidate.active = false;
        Ok(ActiveRenderer {
            submitted: self,
            description: Description {
                profile: Profile::GpuV1,
                generation,
            },
        })
    }

    /// Release the candidate without changing the active execution profile.
    pub fn abort(self) -> io::Result<()> {
        self.candidate.abort()
    }
}

/// A failed activation retaining the submitted candidate for inspection or retry.
#[derive(Debug)]
pub struct ActivationError<'renderer, F: AsFd> {
    submitted: SubmittedCandidate<'renderer, F>,
    error: io::Error,
}

impl<'renderer, F: AsFd> ActivationError<'renderer, F> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_candidate(self) -> SubmittedCandidate<'renderer, F> {
        self.submitted
    }

    /// Discard retry ownership and return the operation error.
    pub fn into_error(self) -> io::Error {
        self.error
    }
}

/// Exclusive access to one active delegated-renderer incarnation.
#[must_use = "retain active renderer ownership while delegated execution is in use"]
#[derive(Debug)]
pub struct ActiveRenderer<'renderer, F: AsFd> {
    submitted: SubmittedCandidate<'renderer, F>,
    description: Description,
}

impl<F: AsFd> ActiveRenderer<'_, F> {
    pub fn description(&self) -> Description {
        self.description
    }

    pub fn configuration(&self) -> OutputConfiguration {
        self.submitted.configuration()
    }
}

impl<F: AsFd> AsFd for ActiveRenderer<'_, F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.submitted.candidate.renderer.as_fd()
    }
}

impl<F: AsFd> Drop for TakeoverCandidate<'_, F> {
    fn drop(&mut self) {
        if self.active {
            let _ = abort(self.renderer.as_fd(), self.id);
        }
    }
}

fn validate_description(query: DrmCastkmsRendererQuery) -> io::Result<Description> {
    if query.version != RENDERER_VERSION || query.flags != 0 || query.reserved != 0 {
        return Err(unsupported("unsupported CastKMS renderer contract"));
    }
    Ok(Description {
        profile: Profile::from_uapi(query.profile)?,
        generation: NonZeroU64::new(query.generation)
            .ok_or_else(|| invalid_data("CastKMS returned a zero execution generation"))?,
    })
}

fn validate_candidate(
    result: DrmCastkmsRendererTakeover,
    expected: Description,
) -> io::Result<CandidateDescription> {
    let profile = Profile::from_uapi(result.profile)?;
    if result.reserved != 0
        || result.execution_generation != expected.generation.get()
        || profile != expected.profile
    {
        return Err(invalid_data("CastKMS returned an inconsistent candidate"));
    }
    Ok(CandidateDescription {
        id: NonZeroU64::new(result.candidate_id)
            .ok_or_else(|| invalid_data("CastKMS returned a zero candidate ID"))?,
        profile,
        configuration: OutputConfiguration {
            width: nonzero(result.width, "CastKMS returned a zero output width")?,
            height: nonzero(result.height, "CastKMS returned a zero output height")?,
            refresh_millihz: nonzero(
                result.refresh_millihz,
                "CastKMS returned a zero output refresh rate",
            )?,
            mode_flags: result.mode_flags,
        },
    })
}

fn validate_startup_image(
    result: DrmCastkmsRendererSnapshot,
    dma_buf: OwnedFd,
    configuration: OutputConfiguration,
) -> io::Result<StartupImage> {
    let descriptor_flags = fcntl(dma_buf.as_raw_fd(), FcntlArg::F_GETFD)?;
    let status_flags = fcntl(dma_buf.as_raw_fd(), FcntlArg::F_GETFL)?;
    let width = nonzero(result.width, "CastKMS returned a zero image width")?;
    let height = nonzero(result.height, "CastKMS returned a zero image height")?;
    let pitch = nonzero(result.pitch, "CastKMS returned a zero image pitch")?;
    let minimum_pitch = width
        .get()
        .checked_mul(4)
        .ok_or_else(|| invalid_data("CastKMS returned an overflowing image width"))?;
    if result.format != DRM_FORMAT_XRGB8888
        || result.modifier != DRM_FORMAT_MOD_LINEAR
        || result.offset != 0
        || result.flags != 0
        || result.reserved != 0
        || descriptor_flags & nix::libc::FD_CLOEXEC == 0
        || status_flags & nix::libc::O_ACCMODE != nix::libc::O_RDONLY
        || width != configuration.width
        || height != configuration.height
        || pitch.get() < minimum_pitch
    {
        return Err(invalid_data(
            "CastKMS returned invalid startup image metadata",
        ));
    }
    Ok(StartupImage {
        dma_buf,
        width,
        height,
        pitch,
        content_serial: NonZeroU64::new(result.content_serial),
    })
}

fn abort(fd: BorrowedFd<'_>, id: NonZeroU64) -> io::Result<()> {
    let request = DrmCastkmsRendererAbortTakeover {
        candidate_id: id.get(),
        ..Default::default()
    };
    // SAFETY: The initialized fixed-width request remains live throughout the
    // synchronous ioctl.
    unsafe { drm_ioctl_castkms_renderer_abort_takeover(fd.as_raw_fd(), &request) }?;
    Ok(())
}

fn nonzero(value: u32, message: &'static str) -> io::Result<NonZeroU32> {
    NonZeroU32::new(value).ok_or_else(|| invalid_data(message))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Debug)]
    struct Owner {
        fd: OwnedFd,
        drops: Rc<Cell<usize>>,
    }

    impl AsFd for Owner {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    fn owner() -> (Owner, Rc<Cell<usize>>) {
        let drops = Rc::new(Cell::new(0));
        let owner = Owner {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            drops: drops.clone(),
        };
        (owner, drops)
    }

    fn expected() -> Description {
        Description {
            profile: Profile::HostV1,
            generation: NonZeroU64::new(7).unwrap(),
        }
    }

    fn configuration() -> OutputConfiguration {
        OutputConfiguration {
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            refresh_millihz: NonZeroU32::new(60_000).unwrap(),
            mode_flags: 0,
        }
    }

    fn submitted_candidate(
        renderer: &mut Renderer<std::fs::File>,
        generation: u64,
    ) -> SubmittedCandidate<'_, std::fs::File> {
        SubmittedCandidate {
            candidate: TakeoverCandidate {
                renderer,
                id: NonZeroU64::new(9).unwrap(),
                profile: Profile::HostV1,
                execution: Description {
                    profile: Profile::HostV1,
                    generation: NonZeroU64::new(generation).unwrap(),
                },
                configuration: configuration(),
                active: true,
            },
        }
    }

    #[test]
    fn failed_validation_drops_the_complete_owner() {
        let (owner, drops) = owner();
        let error = Renderer::from_owner(owner).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ENOTTY));
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn returning_the_owner_preserves_its_type() {
        let (owner, drops) = owner();
        let renderer = Renderer { fd: owner };
        let owner = renderer.into_owner();
        assert_eq!(drops.get(), 0);
        drop(owner);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn description_rejects_unknown_contract_values() {
        let valid = DrmCastkmsRendererQuery {
            version: RENDERER_VERSION,
            profile: EXECUTION_HOST_V1,
            generation: 7,
            ..Default::default()
        };
        assert_eq!(validate_description(valid).unwrap(), expected());
        assert_eq!(
            validate_description(DrmCastkmsRendererQuery {
                profile: EXECUTION_GPU_V1,
                ..valid
            })
            .unwrap()
            .profile(),
            Profile::GpuV1
        );
        for invalid in [
            DrmCastkmsRendererQuery {
                version: RENDERER_VERSION + 1,
                ..valid
            },
            DrmCastkmsRendererQuery { flags: 1, ..valid },
            DrmCastkmsRendererQuery {
                profile: u32::MAX,
                ..valid
            },
            DrmCastkmsRendererQuery {
                generation: 0,
                ..valid
            },
        ] {
            assert!(validate_description(invalid).is_err());
        }
    }

    #[test]
    fn candidate_requires_the_observed_execution() {
        let valid = DrmCastkmsRendererTakeover {
            candidate_id: 9,
            execution_generation: 7,
            profile: EXECUTION_HOST_V1,
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            ..Default::default()
        };
        let candidate = validate_candidate(valid, expected()).unwrap();
        assert_eq!(candidate.id.get(), 9);
        assert_eq!(candidate.configuration, configuration());
        for invalid in [
            DrmCastkmsRendererTakeover {
                candidate_id: 0,
                ..valid
            },
            DrmCastkmsRendererTakeover {
                execution_generation: 8,
                ..valid
            },
            DrmCastkmsRendererTakeover {
                profile: u32::MAX,
                ..valid
            },
            DrmCastkmsRendererTakeover { width: 0, ..valid },
            DrmCastkmsRendererTakeover {
                refresh_millihz: 0,
                ..valid
            },
            DrmCastkmsRendererTakeover {
                reserved: 1,
                ..valid
            },
        ] {
            assert!(validate_candidate(invalid, expected()).is_err());
        }
    }

    #[test]
    fn startup_image_requires_the_candidate_geometry() {
        let make_result = || DrmCastkmsRendererSnapshot {
            dma_buf_fd: 17,
            format: DRM_FORMAT_XRGB8888,
            modifier: DRM_FORMAT_MOD_LINEAR,
            width: 1920,
            height: 1080,
            pitch: 7680,
            content_serial: 11,
            ..Default::default()
        };
        let image = validate_startup_image(
            make_result(),
            std::fs::File::open("/dev/null").unwrap().into(),
            configuration(),
        )
        .unwrap();
        assert_eq!(image.width().get(), 1920);
        assert_eq!(image.height().get(), 1080);
        assert_eq!(image.pitch().get(), 7680);
        assert_eq!(image.content_serial().unwrap().get(), 11);

        let invalid = [
            DrmCastkmsRendererSnapshot {
                format: 0,
                ..make_result()
            },
            DrmCastkmsRendererSnapshot {
                modifier: 1,
                ..make_result()
            },
            DrmCastkmsRendererSnapshot {
                width: 1280,
                ..make_result()
            },
            DrmCastkmsRendererSnapshot {
                pitch: 7679,
                ..make_result()
            },
            DrmCastkmsRendererSnapshot {
                offset: 4,
                ..make_result()
            },
            DrmCastkmsRendererSnapshot {
                flags: 1,
                ..make_result()
            },
        ];
        for result in invalid {
            assert!(validate_startup_image(
                result,
                std::fs::File::open("/dev/null").unwrap().into(),
                configuration(),
            )
            .is_err());
        }

        let writable = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        assert!(validate_startup_image(make_result(), writable.into(), configuration()).is_err());
    }

    #[test]
    fn ordinary_files_reject_renderer_operations() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        assert_eq!(
            renderer.describe().unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        assert_eq!(
            renderer
                .begin_takeover(expected())
                .unwrap_err()
                .raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
    }

    #[test]
    fn failed_activation_returns_the_submitted_candidate() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut renderer = Renderer { fd: file };
        let error = submitted_candidate(&mut renderer, 7)
            .activate()
            .unwrap_err();
        assert_eq!(error.error().raw_os_error(), Some(nix::libc::ENOTTY));
        assert_eq!(error.into_candidate().configuration(), configuration());

        let error = submitted_candidate(&mut renderer, u64::MAX)
            .activate()
            .unwrap_err();
        assert_eq!(error.error().kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.into_candidate().profile(), Profile::HostV1);
    }
}
