//! Renderer backends through an anonymous CastKMS capability.
//!
//! A renderer descriptor grants no modesetting or final-image capture access.
//! It configures one immutable whole-scene backend, names renderer-private storage,
//! reports submitted source reads after KMS selects that backend, and transfers
//! completed private images into separately authorized recipient storage.

mod constraints;
mod image;
mod output;
mod scene;
mod source;

pub use constraints::{ConstraintsFormat, RendererConstraints, StorageProvenance};
pub use image::{RegisteredImage, UnregisterImageError};
pub use output::{OutputChannel, OutputJob, OutputReleaseError, RecipientImage};
pub use scene::{ColorEncoding, ColorOperation, ColorRange, LayerKind, SceneJob, SceneLayer};
pub use source::{FormatModifier, SourceImage, SourcePlane, SourceReleaseError};

use std::fmt;
use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use castkms_sys::{
    drm_ioctl_castkms_create_renderer, drm_ioctl_castkms_renderer_configure,
    drm_ioctl_castkms_renderer_publish, drm_ioctl_castkms_renderer_query,
    drm_ioctl_castkms_renderer_withdraw, DrmCastkmsCreateRenderer, DrmCastkmsRendererConfigure,
    DrmCastkmsRendererFiles, DrmCastkmsRendererPublish, DrmCastkmsRendererPublishResult,
    DrmCastkmsRendererQuery, DrmCastkmsRendererWithdraw, RENDERER_STATE_CONFIGURED,
    RENDERER_STATE_EMPTY, RENDERER_STATE_PUBLISHED, RENDERER_STATE_PUBLISHING,
    RENDERER_STATE_WITHDRAWN, RENDERER_VERSION,
};
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;

fn acquisition_is_idle(error: nix::errno::Errno) -> bool {
    error == nix::errno::Errno::ENODATA
}

fn source_acquisition_is_idle(error: nix::errno::Errno) -> bool {
    matches!(
        error,
        nix::errno::Errno::ESTALE
            | nix::errno::Errno::EAGAIN
            | nix::errno::Errno::ENODATA
            | nix::errno::Errno::ENODEV
    )
}

/// Advisory state returned by the renderer endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointState {
    Empty,
    Configured,
    Publishing,
    Published,
    Withdrawn,
}

/// Advisory endpoint state, independent of accepted KMS state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointDescription {
    state: EndpointState,
    constraints_id: Option<NonZeroU64>,
}

impl EndpointDescription {
    pub fn state(self) -> EndpointState {
        self.state
    }

    pub fn constraints_id(self) -> Option<NonZeroU64> {
        self.constraints_id
    }
}

/// One empty renderer endpoint.
#[derive(Debug)]
pub struct Renderer<F = OwnedFd> {
    endpoint: Endpoint<F>,
}

/// Revocation authority for an administratively issued renderer endpoint.
///
/// Closing the final copy prevents new renderer work through every duplicate
/// endpoint file while leaving cleanup operations available to those files.
#[derive(Debug)]
pub struct Revocation(OwnedFd);

impl AsFd for Revocation {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Renderer {
    /// Adopt and validate a freshly issued renderer descriptor.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Self::from_owner(fd)
    }

    /// Create an endpoint through a privileged, non-master DRM file.
    ///
    /// The kernel requires `CAP_SYS_ADMIN` in the initial user namespace and
    /// binds the result to the independently observed current DRM master for
    /// the selected output. The caller retains `Revocation` for the endpoint's
    /// intended lifetime; dropping it stops fresh renderer work.
    pub fn create_administrative(
        issuer: BorrowedFd<'_>,
        crtc: NonZeroU32,
        connector: NonZeroU32,
    ) -> io::Result<(Self, Revocation)> {
        let mut files = DrmCastkmsRendererFiles {
            renderer_fd: -1,
            revoke_fd: -1,
        };
        let request = DrmCastkmsCreateRenderer {
            crtc_id: crtc.get(),
            connector_id: connector.get(),
            files: (&mut files as *mut DrmCastkmsRendererFiles) as u64,
            ..Default::default()
        };
        // SAFETY: The request and its writable output stay live for the ioctl.
        // On successful return, the kernel installed both fresh descriptors.
        unsafe { drm_ioctl_castkms_create_renderer(issuer.as_raw_fd(), &request) }?;
        let (renderer, revoke) = adopt_created_files(files)?;
        Ok((Self::from_fd(renderer)?, Revocation(revoke)))
    }
}

fn adopt_created_files(files: DrmCastkmsRendererFiles) -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: A successful ioctl transfers ownership of each nonnegative
    // descriptor exactly once. Adopting both before validation closes malformed
    // success output without leaking an installed descriptor.
    let renderer =
        (files.renderer_fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(files.renderer_fd) });
    let revoke = (files.revoke_fd >= 0 && files.revoke_fd != files.renderer_fd)
        .then(|| unsafe { OwnedFd::from_raw_fd(files.revoke_fd) });
    let (Some(renderer), Some(revoke)) = (renderer, revoke) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CastKMS returned invalid renderer descriptors",
        ));
    };
    Ok((renderer, revoke))
}

impl<F: AsFd> Renderer<F> {
    /// Retain an arbitrary descriptor owner after validating its endpoint.
    pub fn from_owner(fd: F) -> io::Result<Self> {
        let endpoint = Endpoint::new(fd);
        let description = endpoint.describe()?;
        if description.state != EndpointState::Empty {
            return Err(invalid_data("renderer endpoint is not empty"));
        }
        Ok(Self { endpoint })
    }

    /// Declare immutable whole-scene constraints and private-pool dimensions.
    pub fn configure(
        self,
        constraints: &RendererConstraints,
        output: Extent,
    ) -> Result<RendererConfiguration<F>, OperationError<Self>> {
        if !constraints.contains_output(output) {
            return Err(OperationError::new(
                self,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private-pool dimensions are outside the renderer constraints",
                ),
            ));
        }
        let bytes = constraints.encode();
        let request = DrmCastkmsRendererConfigure {
            constraints: bytes.as_ptr() as u64,
            constraints_size: bytes
                .len()
                .try_into()
                .expect("bounded renderer constraints fit the ioctl size field"),
            width: output.width(),
            height: output.height(),
            ..Default::default()
        };
        // SAFETY: The initialized request and immutable encoded constraints
        // remain live throughout the synchronous ioctl.
        if let Err(error) =
            unsafe { drm_ioctl_castkms_renderer_configure(self.as_fd().as_raw_fd(), &request) }
        {
            return Err(OperationError::new(self, error.into()));
        }
        Ok(RendererConfiguration {
            endpoint: self.endpoint,
            output,
        })
    }

    /// Return the descriptor owner without changing kernel state.
    pub fn into_owner(self) -> F {
        self.endpoint.fd
    }
}

impl<F: AsFd> AsFd for Renderer<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.endpoint.as_fd()
    }
}

/// One configured renderer whose private resources remain unpublished.
///
/// ```compile_fail
/// use castkms_renderer::RendererConfiguration;
/// use std::os::fd::AsFd;
///
/// fn publish_without_private_preparation<F: AsFd>(configuration: RendererConfiguration<F>) {
///     let _ = configuration.publish(None);
/// }
/// ```
#[must_use = "prepare the renderer configuration or close its endpoint"]
#[derive(Debug)]
pub struct RendererConfiguration<F: AsFd> {
    endpoint: Endpoint<F>,
    output: Extent,
}

impl<F: AsFd> RendererConfiguration<F> {
    pub fn output(&self) -> Extent {
        self.output
    }

    /// Complete private GPU work before making this renderer selectable.
    ///
    /// The image is independent of scene sources and capture destinations. It
    /// proves that the selected device can allocate and execute work for the
    /// configured output extent.
    pub fn prepare_private(
        self,
        device: &Device,
    ) -> Result<PreparedRendererConfiguration<F>, OperationError<Self>> {
        let width = NonZeroU32::new(self.output.width()).expect("output width is nonzero");
        let height = NonZeroU32::new(self.output.height()).expect("output height is nonzero");
        let image = match device.allocate_private(width, height) {
            Ok(image) => image,
            Err(error) => return Err(OperationError::new(self, error)),
        };
        let image = match image.clear_and_wait([0, 0, 0]) {
            Ok(image) => image,
            Err(error) => return Err(OperationError::new(self, error)),
        };
        drop(image);
        Ok(PreparedRendererConfiguration {
            configuration: self,
        })
    }
}

/// A renderer configuration whose private GPU preparation has completed.
#[must_use = "publish or close the prepared renderer"]
#[derive(Debug)]
pub struct PreparedRendererConfiguration<F: AsFd> {
    configuration: RendererConfiguration<F>,
}

impl<F: AsFd> PreparedRendererConfiguration<F> {
    /// Publish a selectable constraints entry after private preparation completes.
    pub fn publish(
        self,
        ready_fence: Option<BorrowedFd<'_>>,
    ) -> Result<PublishedRenderer<F>, PublicationError<F>> {
        let mut result = DrmCastkmsRendererPublishResult::default();
        let request = DrmCastkmsRendererPublish {
            result: (&mut result as *mut DrmCastkmsRendererPublishResult) as u64,
            ready_fence_fd: ready_fence.map_or(-1, |fd| fd.as_raw_fd()),
            ..Default::default()
        };
        // SAFETY: The request, result, and optional borrowed readiness fence remain
        // live throughout the synchronous ioctl.
        if let Err(error) = unsafe {
            drm_ioctl_castkms_renderer_publish(self.configuration.as_fd().as_raw_fd(), &request)
        } {
            return Err(PublicationError::retryable(self, error.into()));
        }
        let Some(constraints_id) = NonZeroU64::new(result.constraints_id) else {
            return Err(PublicationError::terminal(invalid_data(
                "CastKMS returned a zero constraints ID",
            )));
        };
        if result.reserved != [0; 3] {
            return Err(PublicationError::terminal(invalid_data(
                "CastKMS returned reserved publication data",
            )));
        }
        Ok(PublishedRenderer {
            configuration: self.configuration,
            constraints_id,
            withdrawn: false,
        })
    }
}

impl<F: AsFd> AsFd for RendererConfiguration<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.endpoint.as_fd()
    }
}

/// A selectable renderer backend and its source-job channel.
#[must_use = "retain the renderer while its backend or source jobs remain in use"]
#[derive(Debug)]
pub struct PublishedRenderer<F: AsFd> {
    configuration: RendererConfiguration<F>,
    constraints_id: NonZeroU64,
    withdrawn: bool,
}

impl<F: AsFd> PublishedRenderer<F> {
    pub fn output(&self) -> Extent {
        self.configuration.output
    }

    pub fn constraints_id(&self) -> NonZeroU64 {
        self.constraints_id
    }

    /// Stop new selection and source admission without ending cleanup access.
    pub fn withdraw(mut self) -> Result<WithdrawnRenderer<F>, OperationError<Self>> {
        if let Err(error) = withdraw(self.as_fd()) {
            return Err(OperationError::new(self, error));
        }
        self.withdrawn = true;
        Ok(WithdrawnRenderer { published: self })
    }
}

impl<F: AsFd> Drop for PublishedRenderer<F> {
    fn drop(&mut self) {
        if !self.withdrawn {
            let _ = withdraw(self.as_fd());
        }
    }
}

impl<F: AsFd> AsFd for PublishedRenderer<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.configuration.as_fd()
    }
}

/// A withdrawn backend retained for source release and image cleanup.
#[must_use = "retain the endpoint until its outstanding work is resolved"]
#[derive(Debug)]
pub struct WithdrawnRenderer<F: AsFd> {
    published: PublishedRenderer<F>,
}

impl<F: AsFd> WithdrawnRenderer<F> {
    pub fn constraints_id(&self) -> NonZeroU64 {
        self.published.constraints_id
    }
}

impl<F: AsFd> AsFd for WithdrawnRenderer<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.published.as_fd()
    }
}

fn withdraw(fd: BorrowedFd<'_>) -> io::Result<()> {
    let request = DrmCastkmsRendererWithdraw::default();
    // SAFETY: The initialized request remains live throughout the synchronous
    // ioctl.
    unsafe { drm_ioctl_castkms_renderer_withdraw(fd.as_raw_fd(), &request) }?;
    Ok(())
}

/// A failed state transition retaining its input endpoint for retry or close.
pub struct OperationError<T> {
    owner: T,
    error: io::Error,
}

impl<T> OperationError<T> {
    fn new(owner: T, error: io::Error) -> Self {
        Self { owner, error }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (T, io::Error) {
        (self.owner, self.error)
    }

    pub fn into_error(self) -> io::Error {
        self.error
    }
}

impl<T> fmt::Debug for OperationError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<T> fmt::Display for OperationError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl<T: fmt::Debug> std::error::Error for OperationError<T> {}

/// Failed publication, with retry ownership only when nothing was published.
pub struct PublicationError<F: AsFd> {
    retry: Option<PreparedRendererConfiguration<F>>,
    error: io::Error,
}

impl<F: AsFd> PublicationError<F> {
    fn retryable(retry: PreparedRendererConfiguration<F>, error: io::Error) -> Self {
        Self {
            retry: Some(retry),
            error,
        }
    }

    fn terminal(error: io::Error) -> Self {
        Self { retry: None, error }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    /// Return the unpublished configuration when the kernel rejected the ioctl.
    ///
    /// A malformed successful reply closes the endpoint instead, because its
    /// kernel state may already be published and cannot safely be retried.
    pub fn into_retry(self) -> Option<PreparedRendererConfiguration<F>> {
        self.retry
    }

    pub fn into_error(self) -> io::Error {
        self.error
    }
}

impl<F: AsFd> fmt::Debug for PublicationError<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicationError")
            .field("retryable", &self.retry.is_some())
            .field("error", &self.error)
            .finish()
    }
}

impl<F: AsFd> fmt::Display for PublicationError<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl<F: AsFd + fmt::Debug> std::error::Error for PublicationError<F> {}

#[derive(Debug)]
struct Endpoint<F> {
    fd: F,
    image_scope: Arc<()>,
    next_image_id: Option<NonZeroU64>,
    output_channel_issued: bool,
}

impl<F: AsFd> Endpoint<F> {
    fn new(fd: F) -> Self {
        Self {
            fd,
            image_scope: Arc::new(()),
            next_image_id: NonZeroU64::new(1),
            output_channel_issued: false,
        }
    }

    fn describe(&self) -> io::Result<EndpointDescription> {
        let mut query = DrmCastkmsRendererQuery::default();
        // SAFETY: The writable fixed-width response remains live throughout the
        // synchronous ioctl.
        unsafe { drm_ioctl_castkms_renderer_query(self.as_fd().as_raw_fd(), &mut query) }?;
        validate_description(query)
    }
}

impl<F: AsFd> AsFd for Endpoint<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

fn validate_description(query: DrmCastkmsRendererQuery) -> io::Result<EndpointDescription> {
    if query.version != RENDERER_VERSION || query.reserved != [0; 2] {
        return Err(unsupported("unsupported CastKMS renderer contract"));
    }
    let state = match query.state {
        RENDERER_STATE_EMPTY => EndpointState::Empty,
        RENDERER_STATE_CONFIGURED => EndpointState::Configured,
        RENDERER_STATE_PUBLISHING => EndpointState::Publishing,
        RENDERER_STATE_PUBLISHED => EndpointState::Published,
        RENDERER_STATE_WITHDRAWN => EndpointState::Withdrawn,
        _ => return Err(unsupported("unknown CastKMS renderer endpoint state")),
    };
    let constraints_id = NonZeroU64::new(query.constraints_id);
    if matches!(state, EndpointState::Published | EndpointState::Withdrawn)
        != constraints_id.is_some()
    {
        return Err(invalid_data("inconsistent CastKMS renderer endpoint state"));
    }
    Ok(EndpointDescription {
        state,
        constraints_id,
    })
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
    use std::os::fd::IntoRawFd;

    #[test]
    fn failed_administrative_creation_keeps_the_issuer_open() {
        let issuer = std::fs::File::open("/dev/null").unwrap();
        let id = NonZeroU32::new(1).unwrap();
        assert_eq!(
            Renderer::create_administrative(issuer.as_fd(), id, id)
                .unwrap_err()
                .raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        assert!(issuer.metadata().is_ok());
    }

    #[test]
    fn malformed_created_descriptors_are_closed() {
        let (renderer, revoke) = nix::unistd::pipe().unwrap();
        let renderer = renderer.into_raw_fd();
        let revoke = revoke.into_raw_fd();
        assert!(adopt_created_files(DrmCastkmsRendererFiles {
            renderer_fd: renderer,
            revoke_fd: renderer,
        })
        .is_err());
        assert_eq!(
            nix::fcntl::fcntl(renderer, nix::fcntl::FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );
        assert!(adopt_created_files(DrmCastkmsRendererFiles {
            renderer_fd: -1,
            revoke_fd: revoke,
        })
        .is_err());
        assert_eq!(
            nix::fcntl::fcntl(revoke, nix::fcntl::FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );
    }

    #[test]
    fn only_an_empty_acquisition_is_idle() {
        assert!(acquisition_is_idle(nix::errno::Errno::ENODATA));
        assert!(!acquisition_is_idle(nix::errno::Errno::ESTALE));
        assert!(!acquisition_is_idle(nix::errno::Errno::EKEYREVOKED));
        assert!(!acquisition_is_idle(nix::errno::Errno::EBUSY));
    }

    #[test]
    fn unavailable_sources_are_idle() {
        for error in [
            nix::errno::Errno::ESTALE,
            nix::errno::Errno::EAGAIN,
            nix::errno::Errno::ENODATA,
            nix::errno::Errno::ENODEV,
        ] {
            assert!(source_acquisition_is_idle(error));
        }
        for error in [nix::errno::Errno::EKEYREVOKED, nix::errno::Errno::EBUSY] {
            assert!(!source_acquisition_is_idle(error));
        }
    }

    #[test]
    fn endpoint_description_binds_identity_to_published_states() {
        for (state, id) in [
            (RENDERER_STATE_EMPTY, 0),
            (RENDERER_STATE_CONFIGURED, 0),
            (RENDERER_STATE_PUBLISHING, 0),
            (RENDERER_STATE_PUBLISHED, 7),
            (RENDERER_STATE_WITHDRAWN, 7),
        ] {
            let description = validate_description(DrmCastkmsRendererQuery {
                version: RENDERER_VERSION,
                state,
                constraints_id: id,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(
                description.constraints_id().map(NonZeroU64::get),
                NonZeroU64::new(id).map(NonZeroU64::get)
            );
        }
    }

    #[test]
    fn endpoint_description_rejects_mixed_phase_and_identity() {
        for query in [
            DrmCastkmsRendererQuery {
                version: RENDERER_VERSION,
                state: RENDERER_STATE_EMPTY,
                constraints_id: 1,
                ..Default::default()
            },
            DrmCastkmsRendererQuery {
                version: RENDERER_VERSION,
                state: RENDERER_STATE_PUBLISHED,
                ..Default::default()
            },
            DrmCastkmsRendererQuery {
                version: RENDERER_VERSION + 1,
                state: RENDERER_STATE_EMPTY,
                ..Default::default()
            },
        ] {
            assert!(validate_description(query).is_err());
        }
    }
}
