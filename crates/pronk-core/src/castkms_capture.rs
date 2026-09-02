//! Capture capability, stream, and registered-buffer ownership tracking.

use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use castkms_sys::{
    dma_buf_ioctl_export_sync_file, drm_ioctl_castkms_capture_query_caps,
    drm_ioctl_castkms_capture_queue_buffer, drm_ioctl_castkms_capture_register_buffer,
    drm_ioctl_castkms_capture_start, drm_ioctl_castkms_capture_stop,
    drm_ioctl_castkms_capture_unregister_buffer, drm_ioctl_mode_addfb2, drm_ioctl_mode_create_dumb,
    drm_ioctl_mode_destroy_dumb, drm_ioctl_mode_getcrtc, drm_ioctl_mode_rmfb,
    drm_ioctl_prime_handle_to_fd, drm_ioctl_syncobj_create, drm_ioctl_syncobj_destroy,
    drm_ioctl_syncobj_eventfd, drm_ioctl_syncobj_handle_to_fd, drm_ioctl_syncobj_timeline_signal,
    DmaBufExportSyncFile, DrmCastkmsCaptureFormat, DrmCastkmsCaptureQueryCaps,
    DrmCastkmsCaptureQueueBuffer, DrmCastkmsCaptureRegisterBuffer, DrmCastkmsCaptureStart,
    DrmCastkmsCaptureStop, DrmCastkmsCaptureUnregisterBuffer, DrmModeCreateDumb, DrmModeCrtc,
    DrmModeDestroyDumb, DrmModeFbCmd2, DrmPrimeHandle, DrmSyncobjCreate, DrmSyncobjDestroy,
    DrmSyncobjEventfd, DrmSyncobjHandle, DrmSyncobjTimelineArray, CAPTURE_BUFFER_EXPLICIT_SYNC,
    CAPTURE_BUFFER_IMPLICIT_SYNC, CAPTURE_CAP_GRANT_CONTROL_FD, CAPTURE_CAP_GRANT_FD,
    CAPTURE_CAP_IMPLICIT_SYNC, CAPTURE_CAP_SYNCOBJ_TIMELINE, CAPTURE_FRAME_MODE_CHANGED,
    CAPTURE_QUEUE_EXPLICIT_SYNC, CAPTURE_QUEUE_IMPLICIT_SYNC, CAPTURE_START_EXCLUDE_CURSOR,
    CAPTURE_START_EXCLUSIVE, CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR, DMA_BUF_EXPORT_SYNC_WRITE,
    DRM_CLOEXEC, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DRM_RDWR,
    DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_NONE, GRANT_CAPTURE_PIXELS, GRANT_READ_CURSOR,
};
use nix::errno::Errno;
use nix::sys::eventfd::{EfdFlags, EventFd};
use thiserror::Error;
use tokio::io::unix::AsyncFd;

use super::{CaptureFrameEvent, CastKmsClient, CastKmsError, GrantState};

pub const MAX_CAPTURE_FORMATS: usize = 256;
pub const MAX_TRACKED_CAPTURE_BUFFERS: u32 = 64;
pub const MAX_CAPTURE_BUFFER_BYTES: u64 = 1024 * 1024 * 1024;
/// CastKMS exposes one queued-buffer slot per output.
pub const MAX_OUTSTANDING_CAPTURE_REQUESTS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub format: u32,
    pub modifier: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureCapabilities {
    uapi_major: u16,
    uapi_minor: u16,
    crtc_id: NonZeroU32,
    flags: u64,
    formats: Box<[CaptureFormat]>,
    max_registered_buffers: u32,
}

impl CaptureCapabilities {
    pub fn uapi_version(&self) -> (u16, u16) {
        (self.uapi_major, self.uapi_minor)
    }

    pub fn crtc_id(&self) -> NonZeroU32 {
        self.crtc_id
    }

    pub fn flags(&self) -> u64 {
        self.flags
    }

    pub fn formats(&self) -> &[CaptureFormat] {
        &self.formats
    }

    pub fn max_registered_buffers(&self) -> u32 {
        self.max_registered_buffers.min(MAX_TRACKED_CAPTURE_BUFFERS)
    }

    pub fn supports_explicit_sync(&self) -> bool {
        self.flags & CAPTURE_CAP_SYNCOBJ_TIMELINE != 0
    }

    pub fn supports_implicit_sync(&self) -> bool {
        self.flags & CAPTURE_CAP_IMPLICIT_SYNC != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorCaptureMode {
    IncludeInFrame,
    ExcludeFromFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSynchronization {
    Implicit,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStreamInfo {
    pub stream_id: NonZeroU32,
    pub crtc_id: NonZeroU32,
    pub mode_generation: NonZeroU64,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub refresh_hz: NonZeroU32,
    pub cursor_mode: CursorCaptureMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureBufferLayout {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub format: u32,
    pub modifier: u64,
    pub pitch: NonZeroU32,
    pub size: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBufferState {
    Idle,
    Queued,
    Completed,
    ConsumerOwned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureBufferInfo {
    pub stream_id: NonZeroU32,
    pub buffer_id: NonZeroU32,
    pub framebuffer_id: NonZeroU32,
    pub synchronization: CaptureSynchronization,
    pub layout: Option<CaptureBufferLayout>,
    pub state: CaptureBufferState,
}

/// Duplicated timeline descriptors suitable for an in-process media producer.
///
/// The CastKMS client retains the only DRM fd and both private syncobj handles.
/// Closing these exported descriptors cannot mutate grant or capture ownership.
#[derive(Debug)]
pub struct CaptureSyncobjTimelines {
    pub ready: OwnedFd,
    pub reuse: OwnedFd,
}

/// A duplicate-only export of one client-owned capture buffer.
///
/// This deliberately contains no DRM fd, GEM handle, framebuffer ID, or
/// syncobj handle. It can therefore be moved into the PipeWire loop without
/// giving that component a second path to CastKMS.
#[derive(Debug)]
pub struct CaptureBufferExport {
    pub stream_id: NonZeroU32,
    pub buffer_id: NonZeroU32,
    pub layout: CaptureBufferLayout,
    pub dma_buf: OwnedFd,
    pub synchronization: CaptureSynchronization,
    pub timelines: Option<CaptureSyncobjTimelines>,
}

/// The implicit producer fence exported immediately after a capture queue.
///
/// A frame event is metadata only. The destination is safe to access only
/// after this exact fence has become ready.
#[derive(Debug)]
pub struct ImplicitCaptureFence {
    stream_id: NonZeroU32,
    buffer_id: NonZeroU32,
    user_data: NonZeroU64,
    fence: OwnedFd,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CaptureReady {
    stream_id: NonZeroU32,
    buffer_id: NonZeroU32,
    user_data: NonZeroU64,
    ready_point: Option<NonZeroU64>,
}

impl CaptureReady {
    pub fn stream_id(&self) -> NonZeroU32 {
        self.stream_id
    }

    pub fn buffer_id(&self) -> NonZeroU32 {
        self.buffer_id
    }

    pub fn user_data(&self) -> NonZeroU64 {
        self.user_data
    }

    pub fn ready_point(&self) -> Option<NonZeroU64> {
        self.ready_point
    }
}

impl ImplicitCaptureFence {
    pub fn stream_id(&self) -> NonZeroU32 {
        self.stream_id
    }

    pub fn buffer_id(&self) -> NonZeroU32 {
        self.buffer_id
    }

    pub fn user_data(&self) -> NonZeroU64 {
        self.user_data
    }

    /// Wait without blocking a Tokio worker and return unforgeable ownership
    /// evidence for `take_capture_completion`.
    pub async fn wait(self) -> Result<CaptureReady, std::io::Error> {
        let Self {
            stream_id,
            buffer_id,
            user_data,
            fence,
        } = self;
        let fence = AsyncFd::new(fence)?;
        let readiness = fence.readable().await?;
        drop(readiness);
        Ok(CaptureReady {
            stream_id,
            buffer_id,
            user_data,
            ready_point: None,
        })
    }
}

/// A nonblocking eventfd armed for one driver-owned ready timeline point.
///
/// The raw syncobj handle remains private to `CastKmsClient`. Waiting consumes
/// this exact queue's readiness capability and produces the same single-use
/// ownership evidence as the implicit-fence path.
#[derive(Debug)]
pub struct ExplicitCaptureFence {
    stream_id: NonZeroU32,
    buffer_id: NonZeroU32,
    user_data: NonZeroU64,
    ready_point: NonZeroU64,
    event: EventFd,
}

impl ExplicitCaptureFence {
    pub fn stream_id(&self) -> NonZeroU32 {
        self.stream_id
    }

    pub fn buffer_id(&self) -> NonZeroU32 {
        self.buffer_id
    }

    pub fn user_data(&self) -> NonZeroU64 {
        self.user_data
    }

    pub fn ready_point(&self) -> NonZeroU64 {
        self.ready_point
    }

    /// Establish readiness while retaining the single-use completion evidence.
    ///
    /// This is useful when a transport requires the producer to wait before
    /// deciding whether to consume or delegate the completion. The eventual
    /// ownership transition still consumes `self` through [`Self::wait`] or
    /// `CastKmsClient::delegate_explicit_capture_completion`.
    pub async fn wait_ready(&self) -> Result<(), std::io::Error> {
        let event = AsyncFd::new(self.event.as_fd())?;
        let readiness = event.readable().await?;
        drop(readiness);
        Ok(())
    }

    pub async fn wait(self) -> Result<CaptureReady, std::io::Error> {
        self.wait_ready().await?;
        let Self {
            stream_id,
            buffer_id,
            user_data,
            ready_point,
            event,
        } = self;
        drop(event);
        Ok(CaptureReady {
            stream_id,
            buffer_id,
            user_data,
            ready_point: Some(ready_point),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureQueue {
    pub stream_id: NonZeroU32,
    pub buffer_id: NonZeroU32,
    pub user_data: NonZeroU64,
    pub ready_point: Option<NonZeroU64>,
    pub reuse_point: Option<NonZeroU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureCompletion {
    pub queue: CaptureQueue,
    pub frame: CaptureFrameEvent,
}

impl CaptureCompletion {
    pub fn mode_changed(&self) -> bool {
        self.frame.flags & CAPTURE_FRAME_MODE_CHANGED != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRelease {
    pub stream_id: NonZeroU32,
    pub buffer_id: NonZeroU32,
    pub reuse_point: Option<NonZeroU64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredCaptureStream {
    pub stream: CaptureStreamInfo,
    pub buffers: Box<[CaptureBufferInfo]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStopOutcome {
    pub stream: CaptureStreamInfo,
    pub kernel_stream_was_gone: bool,
    pub waiting_buffer_count: usize,
}

/// Evidence that prompted an authoritative grant-state reconciliation.
///
/// A state event is historical evidence as well as a query prompt: a
/// non-active transition invalidates the old media generation even when the
/// subsequent query already reports `Active`. A direct capture error covers
/// the corresponding event-loss case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStateEvidence {
    Query,
    Event(super::GrantStateEvent),
    CaptureInvalidated(Errno),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantCaptureReconciliation {
    NoCapture,
    Retained(CaptureStreamInfo),
    Retired(CaptureStopOutcome),
    Retiring(CaptureStreamInfo),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantStateReconciliation {
    pub grant: super::GrantInfo,
    pub evidence: GrantStateEvidence,
    pub capture: GrantCaptureReconciliation,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error(transparent)]
    Client(#[from] CastKmsError),
    #[error("query CastKMS capture capabilities: {0}")]
    QueryCapabilities(Errno),
    #[error("start CastKMS capture stream: {0}")]
    Start(Errno),
    #[error("stop CastKMS capture stream: {0}")]
    Stop(Errno),
    #[error("register CastKMS capture buffer: {0}")]
    RegisterBuffer(Errno),
    #[error("unregister CastKMS capture buffer: {0}")]
    UnregisterBuffer(Errno),
    #[error("queue CastKMS capture buffer: {0}")]
    QueueBuffer(Errno),
    #[error("query active CastKMS CRTC mode: {0}")]
    QueryCrtc(Errno),
    #[error("create CastKMS dumb capture buffer: {0}")]
    CreateDumbBuffer(Errno),
    #[error("create CastKMS capture framebuffer: {0}")]
    AddFramebuffer(Errno),
    #[error("export CastKMS capture DMA-BUF: {0}")]
    ExportDmaBuf(Errno),
    #[error("duplicate CastKMS capture DMA-BUF: {0}")]
    DuplicateDmaBuf(std::io::Error),
    #[error("export CastKMS implicit capture fence: {0}")]
    ExportImplicitFence(Errno),
    #[error("export CastKMS ready timeline syncobj: {0}")]
    ExportReadySyncobj(Errno),
    #[error("export CastKMS reuse timeline syncobj: {0}")]
    ExportReuseSyncobj(Errno),
    #[error("create CastKMS ready timeline syncobj: {0}")]
    CreateReadySyncobj(Errno),
    #[error("create CastKMS reuse timeline syncobj: {0}")]
    CreateReuseSyncobj(Errno),
    #[error("destroy CastKMS ready timeline syncobj: {0}")]
    DestroyReadySyncobj(Errno),
    #[error("destroy CastKMS reuse timeline syncobj: {0}")]
    DestroyReuseSyncobj(Errno),
    #[error("create explicit-capture eventfd: {0}")]
    CreateExplicitEventFd(Errno),
    #[error("arm explicit-capture ready timeline point: {0}")]
    ArmExplicitFence(Errno),
    #[error("signal explicit-capture reuse timeline point: {0}")]
    SignalReusePoint(Errno),
    #[error("remove CastKMS capture framebuffer: {0}")]
    RemoveFramebuffer(Errno),
    #[error("destroy CastKMS dumb capture buffer: {0}")]
    DestroyDumbBuffer(Errno),
    #[error("CastKMS grant is {0:?}; capture requires Active")]
    GrantNotActive(GrantState),
    #[error("CastKMS capture capabilities are invalid: {0}")]
    InvalidCapabilities(&'static str),
    #[error("CastKMS CRTC mode is invalid: {0}")]
    InvalidCrtcMode(&'static str),
    #[error("CastKMS capture buffer allocation is invalid: {0}")]
    InvalidBufferAllocation(&'static str),
    #[error("CastKMS capture ioctl returned invalid data: {0}")]
    InvalidKernelResult(&'static str),
    #[error("CastKMS reports {actual} capture formats; maximum accepted is {maximum}")]
    TooManyFormats { actual: u32, maximum: usize },
    #[error("CastKMS capture capabilities changed between count and fill queries")]
    CapabilitiesChanged,
    #[error("a capture stream is already active")]
    ActiveStreamExists,
    #[error("retired capture resources must be released before starting another stream")]
    RetiredStreamExists,
    #[error("there is no active capture stream")]
    NoActiveStream,
    #[error("capture stream became stale after a mode change")]
    StaleStream,
    #[error("explicit synchronization is not advertised by the selected CRTC")]
    ExplicitSyncUnsupported,
    #[error("implicit synchronization is not advertised by the selected CRTC")]
    ImplicitSyncUnsupported,
    #[error("capture stream buffer limit {0} has been reached")]
    BufferLimit(u32),
    #[error("CastKMS returned a zero {0}")]
    ZeroKernelIdentifier(&'static str),
    #[error("CastKMS returned duplicate capture buffer ID {0}")]
    DuplicateBufferId(u32),
    #[error("capture buffer {0} is unknown")]
    UnknownBuffer(u32),
    #[error("capture buffer {0} was not allocated by this client")]
    ExternalBuffer(u32),
    #[error("capture buffer {0} does not use implicit synchronization")]
    NotImplicitBuffer(u32),
    #[error("capture buffer {0} does not use explicit synchronization")]
    NotExplicitBuffer(u32),
    #[error("capture buffer {buffer_id} is {actual:?}; expected {expected:?}")]
    InvalidBufferState {
        buffer_id: u32,
        expected: CaptureBufferState,
        actual: CaptureBufferState,
    },
    #[error("the capture stream already has two outstanding requests")]
    CaptureQueueFull,
    #[error("capture user-data value {0} is already in flight")]
    DuplicateUserData(u64),
    #[error("capture-ready evidence names user data {actual}, but completion names {expected}")]
    ReadyUserDataMismatch { expected: u64, actual: u64 },
    #[error("capture-ready evidence names point {actual:?}, but completion names {expected:?}")]
    ReadyPointMismatch {
        expected: Option<NonZeroU64>,
        actual: Option<NonZeroU64>,
    },
    #[error("capture ready timeline point overflowed")]
    ReadyPointOverflow,
    #[error("capture reuse timeline point overflowed")]
    ReusePointOverflow,
    #[error("retired capture stream {0} is unknown")]
    UnknownRetiredStream(u32),
    #[error("retired capture stream still owns non-idle buffers")]
    RetiredStreamBusy,
    #[error("silently canceled capture queue differs from the tracked request")]
    CancelledQueueMismatch,
    #[error("grant-state event names grant {actual}; expected {expected}")]
    ForeignGrantStateEvent { expected: u32, actual: u32 },
    #[error("terminal Revoked state arrived as a grant-state event instead of GRANT_REVOKED")]
    TerminalGrantStateEvent,
    #[error("grant-state event for {state:?} has status {actual}; expected {expected}")]
    InvalidGrantStateEventStatus {
        state: GrantState,
        expected: i32,
        actual: i32,
    },
    #[error("capture errno {0} is not stream-invalidation evidence")]
    InvalidCaptureInvalidation(Errno),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureProtocolError {
    #[error("frame event references unknown capture stream {0}")]
    UnknownStream(u32),
    #[error("frame event references unknown buffer {buffer_id} on stream {stream_id}")]
    UnknownBuffer { stream_id: u32, buffer_id: u32 },
    #[error("frame event found buffer {buffer_id} in {actual:?}, expected Queued")]
    UnexpectedBufferState {
        buffer_id: u32,
        actual: CaptureBufferState,
    },
    #[error("frame event user data {actual} differs from queued value {expected}")]
    UserDataMismatch { expected: u64, actual: u64 },
    #[error(
        "frame event generation {actual} differs from stream generation {expected} without MODE_CHANGED"
    )]
    GenerationMismatch { expected: u64, actual: u64 },
    #[error("frame event contains a zero mode generation")]
    ZeroModeGeneration,
    #[error("mode-change frame status is {actual}; expected -ESTALE")]
    InvalidModeChangeStatus { actual: i32 },
    #[error("mode-change frame retained stale generation {0}")]
    UnchangedModeGeneration(u64),
}

#[derive(Debug, Default)]
pub(super) struct CaptureTracker {
    active: Option<TrackedStream>,
    retired: Option<TrackedStream>,
}

#[derive(Debug)]
struct TrackedStream {
    info: CaptureStreamInfo,
    capabilities: CaptureCapabilities,
    stale: bool,
    buffers: Vec<TrackedBuffer>,
}

#[derive(Debug)]
struct TrackedBuffer {
    id: NonZeroU32,
    framebuffer_id: NonZeroU32,
    synchronization: TrackedSynchronization,
    next_ready_point: u64,
    last_release_point: u64,
    state: TrackedBufferState,
    owned_framebuffer: Option<OwnedFramebuffer>,
}

#[derive(Debug)]
enum TrackedSynchronization {
    Implicit,
    Explicit(OwnedSyncobjPair),
}

impl TrackedSynchronization {
    fn public(&self) -> CaptureSynchronization {
        match self {
            Self::Implicit => CaptureSynchronization::Implicit,
            Self::Explicit(_) => CaptureSynchronization::Explicit,
        }
    }
}

#[derive(Debug)]
struct OwnedSyncobjPair {
    ready_handle: NonZeroU32,
    reuse_handle: NonZeroU32,
}

#[derive(Debug)]
struct OwnedFramebuffer {
    gem_handle: NonZeroU32,
    layout: CaptureBufferLayout,
    dma_buf: OwnedFd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuePoints {
    ready: Option<NonZeroU64>,
    reuse: Option<NonZeroU64>,
    next_ready: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum TrackedBufferState {
    Idle,
    Queued(CaptureQueue),
    Completed(CaptureCompletion),
    ConsumerOwned(CaptureCompletion),
}

impl TrackedBufferState {
    fn public(self) -> CaptureBufferState {
        match self {
            Self::Idle => CaptureBufferState::Idle,
            Self::Queued(_) => CaptureBufferState::Queued,
            Self::Completed(_) => CaptureBufferState::Completed,
            Self::ConsumerOwned(_) => CaptureBufferState::ConsumerOwned,
        }
    }
}

impl CastKmsClient {
    pub fn query_capture_capabilities(
        &self,
        crtc_id: NonZeroU32,
    ) -> Result<CaptureCapabilities, CaptureError> {
        self.require_rights(GRANT_CAPTURE_PIXELS)?;

        let count_query = query_capabilities_ioctl(self, crtc_id, &mut [])?;
        validate_capability_header(self, crtc_id, &count_query)?;
        if count_query.format_count == 0 {
            return Err(CaptureError::InvalidCapabilities("empty format list"));
        }
        if count_query.format_count as usize > MAX_CAPTURE_FORMATS {
            return Err(CaptureError::TooManyFormats {
                actual: count_query.format_count,
                maximum: MAX_CAPTURE_FORMATS,
            });
        }

        let mut raw_formats =
            vec![DrmCastkmsCaptureFormat::default(); count_query.format_count as usize];
        let fill_query = query_capabilities_ioctl(self, crtc_id, &mut raw_formats)?;
        validate_capability_header(self, crtc_id, &fill_query)?;
        if fill_query.uapi_major != count_query.uapi_major
            || fill_query.uapi_minor != count_query.uapi_minor
            || fill_query.flags != count_query.flags
            || fill_query.max_registered_buffers != count_query.max_registered_buffers
            || fill_query.format_count > count_query.format_count
        {
            return Err(CaptureError::CapabilitiesChanged);
        }
        raw_formats.truncate(fill_query.format_count as usize);
        if raw_formats.is_empty() {
            return Err(CaptureError::InvalidCapabilities("empty format list"));
        }

        let mut formats = Vec::with_capacity(raw_formats.len());
        for format in raw_formats {
            if format.format == 0 || format.flags != 0 {
                return Err(CaptureError::InvalidCapabilities("format entry"));
            }
            formats.push(CaptureFormat {
                format: format.format,
                modifier: format.modifier,
            });
        }

        Ok(CaptureCapabilities {
            uapi_major: count_query.uapi_major as u16,
            uapi_minor: count_query.uapi_minor as u16,
            crtc_id,
            flags: count_query.flags,
            formats: formats.into_boxed_slice(),
            max_registered_buffers: count_query.max_registered_buffers,
        })
    }

    pub fn start_capture(
        &mut self,
        capabilities: &CaptureCapabilities,
        cursor_mode: CursorCaptureMode,
    ) -> Result<CaptureStreamInfo, CaptureError> {
        if self.capture.active.is_some() {
            return Err(CaptureError::ActiveStreamExists);
        }
        if self.capture.retired.is_some() {
            return Err(CaptureError::RetiredStreamExists);
        }
        self.require_rights(GRANT_CAPTURE_PIXELS)?;
        if cursor_mode == CursorCaptureMode::IncludeInFrame {
            self.require_rights(GRANT_READ_CURSOR)?;
        }
        let grant = self.query_grant()?;
        if grant.state != GrantState::Active {
            return Err(CaptureError::GrantNotActive(grant.state));
        }
        let (width, height, refresh_hz) = query_crtc_mode(self, capabilities.crtc_id)?;

        let mut flags = CAPTURE_START_EXCLUSIVE;
        if cursor_mode == CursorCaptureMode::ExcludeFromFrame {
            flags |= CAPTURE_START_EXCLUDE_CURSOR;
        }
        let mut args = DrmCastkmsCaptureStart {
            crtc_id: capabilities.crtc_id.get(),
            flags,
            ..DrmCastkmsCaptureStart::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and remains writable
        // for the duration of the synchronous ioctl.
        unsafe { drm_ioctl_castkms_capture_start(self.as_raw_fd(), &mut args) }
            .map_err(CaptureError::Start)?;
        if args.crtc_id != capabilities.crtc_id.get() || args.flags != flags || args.reserved != 0 {
            stop_stream_best_effort(self, args.stream_id);
            return Err(CaptureError::InvalidKernelResult(
                "capture start modified immutable fields",
            ));
        }
        let stream_id = NonZeroU32::new(args.stream_id)
            .ok_or(CaptureError::ZeroKernelIdentifier("stream ID"))?;
        let mode_generation = match NonZeroU64::new(args.mode_generation) {
            Some(generation) => generation,
            None => {
                stop_stream_best_effort(self, stream_id.get());
                return Err(CaptureError::ZeroKernelIdentifier("mode generation"));
            }
        };
        let info = CaptureStreamInfo {
            stream_id,
            crtc_id: capabilities.crtc_id,
            mode_generation,
            width,
            height,
            refresh_hz,
            cursor_mode,
        };
        self.capture.active = Some(TrackedStream {
            info,
            capabilities: capabilities.clone(),
            stale: false,
            buffers: Vec::new(),
        });
        Ok(info)
    }

    pub fn active_capture_stream(&self) -> Option<CaptureStreamInfo> {
        self.capture.active.as_ref().map(|stream| stream.info)
    }

    /// Capture generation already stopped in the kernel but still retaining
    /// buffers that must be drained before their resources can be destroyed.
    pub fn retired_capture_stream(&self) -> Option<CaptureStreamInfo> {
        self.capture.retired.as_ref().map(|stream| stream.info)
    }

    /// Whether the active stream must be stopped and rebuilt before queueing.
    ///
    /// This becomes true after either an asynchronous `MODE_CHANGED` frame or
    /// a synchronous `ESTALE` from buffer registration or queueing.
    pub fn active_capture_requires_restart(&self) -> bool {
        self.capture
            .active
            .as_ref()
            .is_some_and(|stream| stream.stale)
    }

    /// Query the grant authoritatively and reconcile the tracked stream.
    ///
    /// Non-active query results stop and retire an active stream. A validated
    /// non-active event or direct invalidation error also retires it even if a
    /// fast transition means the query already reports `Active` again.
    pub fn reconcile_grant_state(
        &mut self,
        evidence: GrantStateEvidence,
    ) -> Result<GrantStateReconciliation, CaptureError> {
        validate_grant_state_evidence(self.grant_id(), evidence)?;
        let grant = self.query_grant()?;
        let restart = capture_restart_required(grant.state, evidence);
        let capture = if let Some(stream) = self.capture.active.as_ref().map(|stream| stream.info) {
            if restart {
                let stopped = if grant.state.is_terminal() {
                    self.retire_invalidated_capture()?
                } else {
                    self.stop_capture()?
                };
                GrantCaptureReconciliation::Retired(stopped)
            } else {
                GrantCaptureReconciliation::Retained(stream)
            }
        } else if let Some(stream) = self.capture.retired.as_ref().map(|stream| stream.info) {
            GrantCaptureReconciliation::Retiring(stream)
        } else {
            GrantCaptureReconciliation::NoCapture
        };

        Ok(GrantStateReconciliation {
            grant,
            evidence,
            capture,
        })
    }

    pub fn capture_buffers(&self, stream_id: NonZeroU32) -> Vec<CaptureBufferInfo> {
        self.capture
            .stream(stream_id)
            .map_or_else(Vec::new, |stream| stream.buffer_infos())
    }

    pub fn register_capture_buffer(
        &mut self,
        framebuffer_id: NonZeroU32,
        synchronization: CaptureSynchronization,
    ) -> Result<CaptureBufferInfo, CaptureError> {
        let (stream_info, max_buffers, explicit_supported, implicit_supported, buffer_count) = {
            let stream = self
                .capture
                .active
                .as_ref()
                .ok_or(CaptureError::NoActiveStream)?;
            if stream.stale {
                return Err(CaptureError::StaleStream);
            }
            (
                stream.info,
                stream.capabilities.max_registered_buffers(),
                stream.capabilities.supports_explicit_sync(),
                stream.capabilities.supports_implicit_sync(),
                stream.buffers.len() as u32,
            )
        };
        if buffer_count >= max_buffers {
            return Err(CaptureError::BufferLimit(max_buffers));
        }

        let synchronization_mode = synchronization;
        let synchronization = match synchronization {
            CaptureSynchronization::Implicit => {
                if !implicit_supported {
                    return Err(CaptureError::ImplicitSyncUnsupported);
                }
                TrackedSynchronization::Implicit
            }
            CaptureSynchronization::Explicit => {
                if !explicit_supported {
                    return Err(CaptureError::ExplicitSyncUnsupported);
                }
                TrackedSynchronization::Explicit(create_syncobj_pair(self.as_raw_fd())?)
            }
        };
        let (flags, ready_handle, reuse_handle) = match &synchronization {
            TrackedSynchronization::Implicit => (CAPTURE_BUFFER_IMPLICIT_SYNC, 0, 0),
            TrackedSynchronization::Explicit(pair) => (
                CAPTURE_BUFFER_EXPLICIT_SYNC,
                pair.ready_handle.get(),
                pair.reuse_handle.get(),
            ),
        };

        let mut args = DrmCastkmsCaptureRegisterBuffer {
            stream_id: stream_info.stream_id.get(),
            fb_id: framebuffer_id.get(),
            ready_syncobj_handle: ready_handle,
            reuse_syncobj_handle: reuse_handle,
            flags,
            mode_generation: stream_info.mode_generation.get(),
            ..DrmCastkmsCaptureRegisterBuffer::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and remains writable
        // for the duration of the synchronous ioctl.
        if let Err(error) =
            unsafe { drm_ioctl_castkms_capture_register_buffer(self.as_raw_fd(), &mut args) }
        {
            if error == Errno::ESTALE {
                self.capture.mark_active_stale(stream_info.stream_id);
            }
            destroy_syncobj_pair_best_effort(self.as_raw_fd(), synchronization);
            return Err(CaptureError::RegisterBuffer(error));
        }
        let buffer_id = match NonZeroU32::new(args.buffer_id) {
            Some(buffer_id) => buffer_id,
            None => {
                stop_stream_best_effort(self, stream_info.stream_id.get());
                self.capture.retire_active();
                destroy_syncobj_pair_best_effort(self.as_raw_fd(), synchronization);
                return Err(CaptureError::ZeroKernelIdentifier("buffer ID"));
            }
        };
        if args.stream_id != stream_info.stream_id.get()
            || args.fb_id != framebuffer_id.get()
            || args.ready_syncobj_handle != ready_handle
            || args.reuse_syncobj_handle != reuse_handle
            || args.flags != flags
            || args.mode_generation != stream_info.mode_generation.get()
        {
            stop_stream_best_effort(self, stream_info.stream_id.get());
            self.capture.retire_active();
            destroy_syncobj_pair_best_effort(self.as_raw_fd(), synchronization);
            return Err(CaptureError::InvalidKernelResult(
                "buffer registration modified immutable fields",
            ));
        }

        let duplicate = self
            .capture
            .active
            .as_ref()
            .expect("active stream was validated before synchronous ioctl")
            .buffers
            .iter()
            .any(|buffer| buffer.id == buffer_id);
        if duplicate {
            stop_stream_best_effort(self, stream_info.stream_id.get());
            self.capture.retire_active();
            destroy_syncobj_pair_best_effort(self.as_raw_fd(), synchronization);
            return Err(CaptureError::DuplicateBufferId(buffer_id.get()));
        }
        self.capture
            .active
            .as_mut()
            .expect("active stream was validated before synchronous ioctl")
            .buffers
            .push(TrackedBuffer {
                id: buffer_id,
                framebuffer_id,
                synchronization,
                next_ready_point: 1,
                last_release_point: 0,
                state: TrackedBufferState::Idle,
                owned_framebuffer: None,
            });
        Ok(CaptureBufferInfo {
            stream_id: stream_info.stream_id,
            buffer_id,
            framebuffer_id,
            synchronization: synchronization_mode,
            layout: None,
            state: CaptureBufferState::Idle,
        })
    }

    /// Allocate, export, and register a mode-sized linear XRGB8888 buffer.
    ///
    /// The GEM handle and framebuffer remain private to the inherited holder.
    /// Only the resulting DMA-BUF may be borrowed by the local media path.
    pub fn allocate_linear_xrgb8888_buffer(
        &mut self,
        synchronization: CaptureSynchronization,
    ) -> Result<CaptureBufferInfo, CaptureError> {
        let stream_info = {
            let stream = self
                .capture
                .active
                .as_ref()
                .ok_or(CaptureError::NoActiveStream)?;
            if stream.stale {
                return Err(CaptureError::StaleStream);
            }
            if stream.buffers.len() as u32 >= stream.capabilities.max_registered_buffers() {
                return Err(CaptureError::BufferLimit(
                    stream.capabilities.max_registered_buffers(),
                ));
            }
            if !stream.capabilities.formats().iter().any(|format| {
                format.format == DRM_FORMAT_XRGB8888 && format.modifier == DRM_FORMAT_MOD_LINEAR
            }) {
                return Err(CaptureError::InvalidCapabilities(
                    "linear XRGB8888 is unavailable",
                ));
            }
            stream.info
        };

        let mut dumb = DrmModeCreateDumb {
            width: stream_info.width.get(),
            height: stream_info.height.get(),
            bpp: 32,
            ..DrmModeCreateDumb::default()
        };
        // SAFETY: `dumb` has the standard DRM UAPI layout and remains writable
        // for the duration of the synchronous ioctl.
        unsafe { drm_ioctl_mode_create_dumb(self.as_raw_fd(), &mut dumb) }
            .map_err(CaptureError::CreateDumbBuffer)?;
        let gem_handle = match validate_dumb_allocation(stream_info, &dumb) {
            Ok(handle) => handle,
            Err(error) => {
                destroy_dumb_best_effort(self.as_raw_fd(), dumb.handle);
                return Err(error);
            }
        };
        let layout = CaptureBufferLayout {
            width: stream_info.width,
            height: stream_info.height,
            format: DRM_FORMAT_XRGB8888,
            modifier: DRM_FORMAT_MOD_LINEAR,
            pitch: NonZeroU32::new(dumb.pitch)
                .expect("validated dumb allocation has a nonzero pitch"),
            size: NonZeroU64::new(dumb.size).expect("validated dumb allocation has a nonzero size"),
        };

        let mut framebuffer = DrmModeFbCmd2 {
            width: layout.width.get(),
            height: layout.height.get(),
            pixel_format: layout.format,
            handles: [gem_handle.get(), 0, 0, 0],
            pitches: [layout.pitch.get(), 0, 0, 0],
            modifier: [layout.modifier, 0, 0, 0],
            ..DrmModeFbCmd2::default()
        };
        // SAFETY: `framebuffer` has the standard DRM UAPI layout and remains
        // writable for the duration of the synchronous ioctl.
        if let Err(error) = unsafe { drm_ioctl_mode_addfb2(self.as_raw_fd(), &mut framebuffer) } {
            destroy_dumb_best_effort(self.as_raw_fd(), gem_handle.get());
            return Err(CaptureError::AddFramebuffer(error));
        }
        let framebuffer_id = match validate_framebuffer_result(&framebuffer, layout, gem_handle) {
            Ok(framebuffer_id) => framebuffer_id,
            Err(error) => {
                remove_framebuffer_best_effort(self.as_raw_fd(), framebuffer.fb_id);
                destroy_dumb_best_effort(self.as_raw_fd(), gem_handle.get());
                return Err(error);
            }
        };

        let mut prime = DrmPrimeHandle {
            handle: gem_handle.get(),
            flags: DRM_CLOEXEC | DRM_RDWR,
            fd: -1,
        };
        // SAFETY: `prime` has the standard DRM UAPI layout and remains
        // writable for the duration of the synchronous ioctl.
        if let Err(error) = unsafe { drm_ioctl_prime_handle_to_fd(self.as_raw_fd(), &mut prime) } {
            remove_framebuffer_best_effort(self.as_raw_fd(), framebuffer_id.get());
            destroy_dumb_best_effort(self.as_raw_fd(), gem_handle.get());
            return Err(CaptureError::ExportDmaBuf(error));
        }
        let dma_buf = match owned_fd_from_kernel(
            prime.fd,
            prime.handle == gem_handle.get() && prime.flags == DRM_CLOEXEC | DRM_RDWR,
            "DMA-BUF export",
        ) {
            Ok(fd) => fd,
            Err(error) => {
                remove_framebuffer_best_effort(self.as_raw_fd(), framebuffer_id.get());
                destroy_dumb_best_effort(self.as_raw_fd(), gem_handle.get());
                return Err(error);
            }
        };
        let owned_framebuffer = OwnedFramebuffer {
            gem_handle,
            layout,
            dma_buf,
        };

        let registered = match self.register_capture_buffer(framebuffer_id, synchronization) {
            Ok(registered) => registered,
            Err(error) => {
                cleanup_owned_framebuffer_best_effort(
                    self.as_raw_fd(),
                    framebuffer_id,
                    owned_framebuffer,
                );
                return Err(error);
            }
        };
        let buffer = self
            .capture
            .active
            .as_mut()
            .expect("registration retained the active stream")
            .buffer_mut(registered.buffer_id)
            .expect("registration retained the returned buffer");
        buffer.owned_framebuffer = Some(owned_framebuffer);
        Ok(buffer.info(registered.stream_id))
    }

    /// Borrow the exported DMA-BUF without exposing the holder, GEM handle, or
    /// framebuffer namespace to another process.
    pub fn capture_dma_buf(
        &self,
        stream_id: NonZeroU32,
        buffer_id: NonZeroU32,
    ) -> Result<BorrowedFd<'_>, CaptureError> {
        let stream = self
            .capture
            .stream(stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
        let buffer = stream
            .buffer(buffer_id)
            .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
        let owned = buffer
            .owned_framebuffer
            .as_ref()
            .ok_or(CaptureError::ExternalBuffer(buffer_id.get()))?;
        Ok(owned.dma_buf.as_fd())
    }

    /// Duplicate one owned capture buffer for the local media plane.
    ///
    /// Explicit synchronization exports opaque syncobj descriptors while the
    /// raw handles and all timeline mutation remain private to this client.
    pub fn export_capture_buffer(
        &self,
        stream_id: NonZeroU32,
        buffer_id: NonZeroU32,
    ) -> Result<CaptureBufferExport, CaptureError> {
        let stream = self
            .capture
            .stream(stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
        let buffer = stream
            .buffer(buffer_id)
            .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
        let owned = buffer
            .owned_framebuffer
            .as_ref()
            .ok_or(CaptureError::ExternalBuffer(buffer_id.get()))?;
        let dma_buf = owned
            .dma_buf
            .try_clone()
            .map_err(CaptureError::DuplicateDmaBuf)?;
        let (synchronization, timelines) = match &buffer.synchronization {
            TrackedSynchronization::Implicit => (CaptureSynchronization::Implicit, None),
            TrackedSynchronization::Explicit(pair) => {
                let ready = export_syncobj_fd(
                    self.as_raw_fd(),
                    pair.ready_handle,
                    CaptureError::ExportReadySyncobj,
                )?;
                let reuse = export_syncobj_fd(
                    self.as_raw_fd(),
                    pair.reuse_handle,
                    CaptureError::ExportReuseSyncobj,
                )?;
                (
                    CaptureSynchronization::Explicit,
                    Some(CaptureSyncobjTimelines { ready, reuse }),
                )
            }
        };

        Ok(CaptureBufferExport {
            stream_id,
            buffer_id,
            layout: owned.layout,
            dma_buf,
            synchronization,
            timelines,
        })
    }

    /// Export the implicit producer fence installed by the most recent queue.
    ///
    /// Call this immediately after `queue_capture_buffer`, before awaiting DRM
    /// events. The returned object carries the queue identity into the
    /// authoritative asynchronous fence wait.
    pub fn export_implicit_capture_fence(
        &self,
        stream_id: NonZeroU32,
        buffer_id: NonZeroU32,
    ) -> Result<ImplicitCaptureFence, CaptureError> {
        let stream = self
            .capture
            .stream(stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
        let buffer = stream
            .buffer(buffer_id)
            .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
        if !matches!(buffer.synchronization, TrackedSynchronization::Implicit) {
            return Err(CaptureError::NotImplicitBuffer(buffer_id.get()));
        }
        let queue = match buffer.state {
            TrackedBufferState::Queued(queue) => queue,
            state => {
                return Err(CaptureError::InvalidBufferState {
                    buffer_id: buffer_id.get(),
                    expected: CaptureBufferState::Queued,
                    actual: state.public(),
                })
            }
        };
        let owned = buffer
            .owned_framebuffer
            .as_ref()
            .ok_or(CaptureError::ExternalBuffer(buffer_id.get()))?;
        let mut export = DmaBufExportSyncFile {
            flags: DMA_BUF_EXPORT_SYNC_WRITE,
            fd: -1,
        };
        // SAFETY: `export` has the DMA-BUF UAPI layout and remains writable
        // for the duration of the synchronous ioctl. `owned` keeps the
        // DMA-BUF alive.
        unsafe { dma_buf_ioctl_export_sync_file(owned.dma_buf.as_raw_fd(), &mut export) }
            .map_err(CaptureError::ExportImplicitFence)?;
        let fence = owned_fd_from_kernel(
            export.fd,
            export.flags == DMA_BUF_EXPORT_SYNC_WRITE,
            "implicit fence export",
        )?;
        Ok(ImplicitCaptureFence {
            stream_id,
            buffer_id,
            user_data: queue.user_data,
            fence,
        })
    }

    /// Arm a fresh eventfd for the explicit ready point created by the most
    /// recent queue. The syncobj handles never leave the client.
    pub fn arm_explicit_capture_fence(
        &self,
        stream_id: NonZeroU32,
        buffer_id: NonZeroU32,
    ) -> Result<ExplicitCaptureFence, CaptureError> {
        let stream = self
            .capture
            .stream(stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
        let buffer = stream
            .buffer(buffer_id)
            .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
        let pair = match &buffer.synchronization {
            TrackedSynchronization::Explicit(pair) => pair,
            TrackedSynchronization::Implicit => {
                return Err(CaptureError::NotExplicitBuffer(buffer_id.get()))
            }
        };
        let queue = match buffer.state {
            TrackedBufferState::Queued(queue) => queue,
            state => {
                return Err(CaptureError::InvalidBufferState {
                    buffer_id: buffer_id.get(),
                    expected: CaptureBufferState::Queued,
                    actual: state.public(),
                })
            }
        };
        let ready_point = queue.ready_point.ok_or(CaptureError::InvalidKernelResult(
            "explicit queue has no ready point",
        ))?;
        let event = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .map_err(CaptureError::CreateExplicitEventFd)?;
        let mut args = DrmSyncobjEventfd {
            handle: pair.ready_handle.get(),
            point: ready_point.get(),
            fd: event.as_raw_fd(),
            ..DrmSyncobjEventfd::default()
        };
        // SAFETY: `args` has the standard DRM UAPI layout, names a live
        // client-owned syncobj and eventfd, and remains writable for the
        // duration of the synchronous ioctl.
        unsafe { drm_ioctl_syncobj_eventfd(self.as_raw_fd(), &mut args) }
            .map_err(CaptureError::ArmExplicitFence)?;
        if args.handle != pair.ready_handle.get()
            || args.flags != 0
            || args.point != ready_point.get()
            || args.fd != event.as_raw_fd()
            || args.pad != 0
        {
            return Err(CaptureError::InvalidKernelResult(
                "syncobj eventfd registration modified immutable fields",
            ));
        }
        Ok(ExplicitCaptureFence {
            stream_id,
            buffer_id,
            user_data: queue.user_data,
            ready_point,
            event,
        })
    }

    pub fn unregister_capture_buffer(
        &mut self,
        buffer_id: NonZeroU32,
    ) -> Result<CaptureBufferInfo, CaptureError> {
        let (stream_info, buffer_index, info) = {
            let stream = self
                .capture
                .active
                .as_ref()
                .ok_or(CaptureError::NoActiveStream)?;
            let index = stream
                .buffer_index(buffer_id)
                .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
            let buffer = &stream.buffers[index];
            require_buffer_state(buffer, CaptureBufferState::Idle)?;
            (stream.info, index, buffer.info(stream.info.stream_id))
        };
        let args = DrmCastkmsCaptureUnregisterBuffer {
            stream_id: stream_info.stream_id.get(),
            buffer_id: buffer_id.get(),
            ..DrmCastkmsCaptureUnregisterBuffer::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and remains valid for
        // the duration of the synchronous ioctl.
        unsafe { drm_ioctl_castkms_capture_unregister_buffer(self.as_raw_fd(), &args) }
            .map_err(CaptureError::UnregisterBuffer)?;
        let buffer = self
            .capture
            .active
            .as_mut()
            .expect("active stream was validated before synchronous ioctl")
            .buffers
            .remove(buffer_index);
        cleanup_tracked_buffer_resources(self.as_raw_fd(), buffer)?;
        Ok(info)
    }

    pub fn queue_capture_buffer(
        &mut self,
        buffer_id: NonZeroU32,
        user_data: NonZeroU64,
    ) -> Result<CaptureQueue, CaptureError> {
        let (stream_info, synchronization, ready_point, reuse_point, next_ready_point) = {
            let stream = self
                .capture
                .active
                .as_ref()
                .ok_or(CaptureError::NoActiveStream)?;
            if stream.stale {
                return Err(CaptureError::StaleStream);
            }
            if !stream.has_capture_queue_capacity() {
                return Err(CaptureError::CaptureQueueFull);
            }
            if stream.buffers.iter().any(|buffer| {
                matches!(
                    buffer.state,
                    TrackedBufferState::Queued(queue)
                        | TrackedBufferState::Completed(CaptureCompletion { queue, .. })
                        | TrackedBufferState::ConsumerOwned(CaptureCompletion { queue, .. })
                        if queue.user_data == user_data
                )
            }) {
                return Err(CaptureError::DuplicateUserData(user_data.get()));
            }
            let buffer = stream
                .buffer(buffer_id)
                .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
            require_buffer_state(buffer, CaptureBufferState::Idle)?;
            let points = queue_points(buffer)?;
            (
                stream.info,
                buffer.synchronization.public(),
                points.ready,
                points.reuse,
                points.next_ready,
            )
        };

        let flags = match synchronization {
            CaptureSynchronization::Implicit => CAPTURE_QUEUE_IMPLICIT_SYNC,
            CaptureSynchronization::Explicit => CAPTURE_QUEUE_EXPLICIT_SYNC,
        };
        let queue = CaptureQueue {
            stream_id: stream_info.stream_id,
            buffer_id,
            user_data,
            ready_point,
            reuse_point,
        };
        let args = DrmCastkmsCaptureQueueBuffer {
            stream_id: stream_info.stream_id.get(),
            buffer_id: buffer_id.get(),
            flags,
            user_data: user_data.get(),
            mode_generation: stream_info.mode_generation.get(),
            ready_point: ready_point.map_or(0, NonZeroU64::get),
            reuse_point: reuse_point.map_or(0, NonZeroU64::get),
            ..DrmCastkmsCaptureQueueBuffer::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and remains valid for
        // the duration of the synchronous ioctl.
        if let Err(error) =
            unsafe { drm_ioctl_castkms_capture_queue_buffer(self.as_raw_fd(), &args) }
        {
            if error == Errno::ESTALE {
                self.capture.mark_active_stale(stream_info.stream_id);
            }
            return Err(CaptureError::QueueBuffer(error));
        }

        let stream = self
            .capture
            .active
            .as_mut()
            .expect("active stream was validated before synchronous ioctl");
        let buffer = stream
            .buffer_mut(buffer_id)
            .expect("buffer was validated before synchronous ioctl");
        if let Some(next_ready_point) = next_ready_point {
            buffer.next_ready_point = next_ready_point;
        }
        buffer.state = TrackedBufferState::Queued(queue);
        Ok(queue)
    }

    pub fn take_capture_completion(
        &mut self,
        ready: CaptureReady,
    ) -> Result<CaptureCompletion, CaptureError> {
        let stream_id = ready.stream_id;
        let buffer_id = ready.buffer_id;
        let stream = self
            .capture
            .stream_mut(stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
        let buffer = stream
            .buffer_mut(buffer_id)
            .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
        let completion = match buffer.state {
            TrackedBufferState::Completed(completion) => completion,
            state => {
                return Err(CaptureError::InvalidBufferState {
                    buffer_id: buffer_id.get(),
                    expected: CaptureBufferState::Completed,
                    actual: state.public(),
                })
            }
        };
        if completion.queue.user_data != ready.user_data {
            return Err(CaptureError::ReadyUserDataMismatch {
                expected: completion.queue.user_data.get(),
                actual: ready.user_data.get(),
            });
        }
        if completion.queue.ready_point != ready.ready_point {
            return Err(CaptureError::ReadyPointMismatch {
                expected: completion.queue.ready_point,
                actual: ready.ready_point,
            });
        }
        buffer.state = TrackedBufferState::ConsumerOwned(completion);
        Ok(completion)
    }

    /// Delegate an explicit completion without waiting for its ready point.
    ///
    /// The consumer must receive this buffer's exported ready timeline and the
    /// exact `queue.ready_point` as acquire metadata before it can access the
    /// DMA-BUF. This is the zero-wait handoff used by the PipeWire producer.
    pub fn delegate_explicit_capture_completion(
        &mut self,
        fence: ExplicitCaptureFence,
    ) -> Result<CaptureCompletion, CaptureError> {
        let ExplicitCaptureFence {
            stream_id,
            buffer_id,
            user_data,
            ready_point,
            event,
        } = fence;
        drop(event);
        self.take_capture_completion(CaptureReady {
            stream_id,
            buffer_id,
            user_data,
            ready_point: Some(ready_point),
        })
    }

    /// Return a consumer-owned buffer to the capture queue.
    ///
    /// Explicit buffers advance and signal their private reuse timeline here;
    /// callers cannot choose a handle or forge a point.
    pub fn release_capture_buffer(
        &mut self,
        stream_id: NonZeroU32,
        buffer_id: NonZeroU32,
    ) -> Result<CaptureRelease, CaptureError> {
        let (reuse_handle, reuse_point) = {
            let stream = self
                .capture
                .stream(stream_id)
                .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
            let buffer = stream
                .buffer(buffer_id)
                .ok_or(CaptureError::UnknownBuffer(buffer_id.get()))?;
            require_buffer_state(buffer, CaptureBufferState::ConsumerOwned)?;
            let point = next_reuse_point(buffer)?;
            match &buffer.synchronization {
                TrackedSynchronization::Implicit => (None, None),
                TrackedSynchronization::Explicit(pair) => (Some(pair.reuse_handle), point),
            }
        };

        if let (Some(handle), Some(point)) = (reuse_handle, reuse_point) {
            signal_syncobj_point(self.as_raw_fd(), handle, point)?;
        }

        let stream = self
            .capture
            .stream_mut(stream_id)
            .expect("stream was validated before the synchronous signal ioctl");
        let buffer = stream
            .buffer_mut(buffer_id)
            .expect("buffer was validated before the synchronous signal ioctl");
        if let Some(point) = reuse_point {
            buffer.last_release_point = point.get();
        }
        buffer.state = TrackedBufferState::Idle;
        Ok(CaptureRelease {
            stream_id,
            buffer_id,
            reuse_point,
        })
    }

    /// Retire a queued request canceled by a successful capture-stop ioctl.
    ///
    /// CastKMS synchronously completes the producer fence but removes the DRM
    /// event for `ECANCELED`. The caller must first wait for that exact queue's
    /// producer fence and drain all already-published frame events. A queue
    /// still tracked as queued after those two proofs is the silent
    /// cancellation and never became consumer-owned.
    pub fn retire_cancelled_capture_buffer(
        &mut self,
        queue: CaptureQueue,
    ) -> Result<(), CaptureError> {
        let stream = self
            .capture
            .retired
            .as_mut()
            .filter(|stream| stream.info.stream_id == queue.stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(queue.stream_id.get()))?;
        let buffer = stream
            .buffer_mut(queue.buffer_id)
            .ok_or(CaptureError::UnknownBuffer(queue.buffer_id.get()))?;
        match buffer.state {
            TrackedBufferState::Queued(actual) if actual == queue => {
                buffer.state = TrackedBufferState::Idle;
                Ok(())
            }
            TrackedBufferState::Queued(_) => Err(CaptureError::CancelledQueueMismatch),
            state => Err(CaptureError::InvalidBufferState {
                buffer_id: queue.buffer_id.get(),
                expected: CaptureBufferState::Queued,
                actual: state.public(),
            }),
        }
    }

    pub fn stop_capture(&mut self) -> Result<CaptureStopOutcome, CaptureError> {
        let stream = self
            .capture
            .active
            .as_ref()
            .ok_or(CaptureError::NoActiveStream)?;
        let stream_info = stream.info;
        let args = DrmCastkmsCaptureStop {
            stream_id: stream_info.stream_id.get(),
            ..DrmCastkmsCaptureStop::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and remains valid for
        // the duration of the synchronous ioctl.
        let kernel_stream_was_gone =
            match unsafe { drm_ioctl_castkms_capture_stop(self.as_raw_fd(), &args) } {
                Ok(_) => false,
                Err(Errno::ENOENT) => true,
                Err(error) => return Err(CaptureError::Stop(error)),
            };
        let waiting_buffer_count = self.capture.retire_active();
        Ok(CaptureStopOutcome {
            stream: stream_info,
            kernel_stream_was_gone,
            waiting_buffer_count,
        })
    }

    /// Retire local stream ownership after an authoritative state query proves
    /// that the kernel already invalidated it.
    pub fn retire_invalidated_capture(&mut self) -> Result<CaptureStopOutcome, CaptureError> {
        let stream = self
            .capture
            .active
            .as_ref()
            .ok_or(CaptureError::NoActiveStream)?;
        let stream_info = stream.info;
        let waiting_buffer_count = self.capture.retire_active();
        Ok(CaptureStopOutcome {
            stream: stream_info,
            kernel_stream_was_gone: true,
            waiting_buffer_count,
        })
    }

    pub fn finish_retired_capture(
        &mut self,
        stream_id: NonZeroU32,
    ) -> Result<RetiredCaptureStream, CaptureError> {
        let stream = self
            .capture
            .retired
            .as_ref()
            .filter(|stream| stream.info.stream_id == stream_id)
            .ok_or(CaptureError::UnknownRetiredStream(stream_id.get()))?;
        if stream
            .buffers
            .iter()
            .any(|buffer| buffer.state.public() != CaptureBufferState::Idle)
        {
            return Err(CaptureError::RetiredStreamBusy);
        }
        let stream = self
            .capture
            .retired
            .take()
            .expect("retired stream was validated");
        let retired = RetiredCaptureStream {
            stream: stream.info,
            buffers: stream.buffer_infos().into_boxed_slice(),
        };
        let mut cleanup_error = None;
        for buffer in stream.buffers {
            if let Err(error) = cleanup_tracked_buffer_resources(self.as_raw_fd(), buffer) {
                cleanup_error.get_or_insert(error);
            }
        }
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        Ok(retired)
    }

    pub(super) fn record_capture_frame(
        &mut self,
        event: CaptureFrameEvent,
    ) -> Result<(), CaptureProtocolError> {
        self.capture.record_frame(event)
    }
}

impl CaptureTracker {
    fn stream(&self, stream_id: NonZeroU32) -> Option<&TrackedStream> {
        self.active
            .as_ref()
            .filter(|stream| stream.info.stream_id == stream_id)
            .or_else(|| {
                self.retired
                    .as_ref()
                    .filter(|stream| stream.info.stream_id == stream_id)
            })
    }

    fn stream_mut(&mut self, stream_id: NonZeroU32) -> Option<&mut TrackedStream> {
        if self
            .active
            .as_ref()
            .is_some_and(|stream| stream.info.stream_id == stream_id)
        {
            return self.active.as_mut();
        }
        self.retired
            .as_mut()
            .filter(|stream| stream.info.stream_id == stream_id)
    }

    fn retire_active(&mut self) -> usize {
        let stream = self
            .active
            .take()
            .expect("caller verified an active capture stream");
        let waiting = stream
            .buffers
            .iter()
            .filter(|buffer| buffer.state.public() != CaptureBufferState::Idle)
            .count();
        if stream.buffers.is_empty() {
            debug_assert!(self.retired.is_none());
        } else {
            debug_assert!(self.retired.is_none());
            self.retired = Some(stream);
        }
        waiting
    }

    fn mark_active_stale(&mut self, stream_id: NonZeroU32) {
        if let Some(stream) = self
            .active
            .as_mut()
            .filter(|stream| stream.info.stream_id == stream_id)
        {
            stream.stale = true;
        }
    }

    fn record_frame(&mut self, event: CaptureFrameEvent) -> Result<(), CaptureProtocolError> {
        let stream_id = NonZeroU32::new(event.stream_id)
            .ok_or(CaptureProtocolError::UnknownStream(event.stream_id))?;
        let buffer_id =
            NonZeroU32::new(event.buffer_id).ok_or(CaptureProtocolError::UnknownBuffer {
                stream_id: event.stream_id,
                buffer_id: event.buffer_id,
            })?;
        let stream = self
            .stream_mut(stream_id)
            .ok_or(CaptureProtocolError::UnknownStream(event.stream_id))?;
        let stream_generation = stream.info.mode_generation.get();
        if event.mode_generation == 0 {
            return Err(CaptureProtocolError::ZeroModeGeneration);
        }
        if event.flags & CAPTURE_FRAME_MODE_CHANGED != 0 {
            if event.status != -(Errno::ESTALE as i32) {
                return Err(CaptureProtocolError::InvalidModeChangeStatus {
                    actual: event.status,
                });
            }
            if event.mode_generation == stream_generation {
                return Err(CaptureProtocolError::UnchangedModeGeneration(
                    event.mode_generation,
                ));
            }
        }
        if event.status == 0
            && event.flags & CAPTURE_FRAME_MODE_CHANGED == 0
            && event.mode_generation != stream_generation
        {
            return Err(CaptureProtocolError::GenerationMismatch {
                expected: stream_generation,
                actual: event.mode_generation,
            });
        }
        let buffer = stream
            .buffer_mut(buffer_id)
            .ok_or(CaptureProtocolError::UnknownBuffer {
                stream_id: event.stream_id,
                buffer_id: event.buffer_id,
            })?;
        let queue = match buffer.state {
            TrackedBufferState::Queued(queue) => queue,
            state => {
                return Err(CaptureProtocolError::UnexpectedBufferState {
                    buffer_id: event.buffer_id,
                    actual: state.public(),
                })
            }
        };
        if event.user_data != queue.user_data.get() {
            return Err(CaptureProtocolError::UserDataMismatch {
                expected: queue.user_data.get(),
                actual: event.user_data,
            });
        }
        buffer.state = TrackedBufferState::Completed(CaptureCompletion {
            queue,
            frame: event,
        });
        if event.flags & CAPTURE_FRAME_MODE_CHANGED != 0 {
            stream.stale = true;
        }
        Ok(())
    }
}

fn validate_grant_state_evidence(
    expected_grant_id: u32,
    evidence: GrantStateEvidence,
) -> Result<(), CaptureError> {
    match evidence {
        GrantStateEvidence::Query => Ok(()),
        GrantStateEvidence::Event(event) => {
            if event.grant_id != expected_grant_id {
                return Err(CaptureError::ForeignGrantStateEvent {
                    expected: expected_grant_id,
                    actual: event.grant_id,
                });
            }
            let expected = match event.state {
                GrantState::Pending => -(Errno::ENOLINK as i32),
                GrantState::Active => 0,
                GrantState::SuspendedNoMaster | GrantState::SuspendedOtherMaster => {
                    -(Errno::EAGAIN as i32)
                }
                GrantState::SuspendedForeignContent => -(Errno::ESTALE as i32),
                GrantState::Revoked => return Err(CaptureError::TerminalGrantStateEvent),
            };
            if event.status != expected {
                return Err(CaptureError::InvalidGrantStateEventStatus {
                    state: event.state,
                    expected,
                    actual: event.status,
                });
            }
            Ok(())
        }
        GrantStateEvidence::CaptureInvalidated(
            Errno::ENOENT | Errno::EAGAIN | Errno::ESTALE | Errno::ENOLINK | Errno::ENOTCONN,
        ) => Ok(()),
        GrantStateEvidence::CaptureInvalidated(error) => {
            Err(CaptureError::InvalidCaptureInvalidation(error))
        }
    }
}

fn capture_restart_required(state: GrantState, evidence: GrantStateEvidence) -> bool {
    state != GrantState::Active
        || matches!(
            evidence,
            GrantStateEvidence::Event(super::GrantStateEvent {
                state: GrantState::Pending
                    | GrantState::SuspendedNoMaster
                    | GrantState::SuspendedOtherMaster
                    | GrantState::SuspendedForeignContent
                    | GrantState::Revoked,
                ..
            }) | GrantStateEvidence::CaptureInvalidated(_)
        )
}

impl TrackedStream {
    fn has_capture_queue_capacity(&self) -> bool {
        self.buffers
            .iter()
            .filter(|buffer| matches!(buffer.state, TrackedBufferState::Queued(_)))
            .count()
            < MAX_OUTSTANDING_CAPTURE_REQUESTS
    }

    fn buffer(&self, buffer_id: NonZeroU32) -> Option<&TrackedBuffer> {
        self.buffers.iter().find(|buffer| buffer.id == buffer_id)
    }

    fn buffer_mut(&mut self, buffer_id: NonZeroU32) -> Option<&mut TrackedBuffer> {
        self.buffers
            .iter_mut()
            .find(|buffer| buffer.id == buffer_id)
    }

    fn buffer_index(&self, buffer_id: NonZeroU32) -> Option<usize> {
        self.buffers
            .iter()
            .position(|buffer| buffer.id == buffer_id)
    }

    fn buffer_infos(&self) -> Vec<CaptureBufferInfo> {
        self.buffers
            .iter()
            .map(|buffer| buffer.info(self.info.stream_id))
            .collect()
    }
}

impl TrackedBuffer {
    fn info(&self, stream_id: NonZeroU32) -> CaptureBufferInfo {
        CaptureBufferInfo {
            stream_id,
            buffer_id: self.id,
            framebuffer_id: self.framebuffer_id,
            synchronization: self.synchronization.public(),
            layout: self
                .owned_framebuffer
                .as_ref()
                .map(|framebuffer| framebuffer.layout),
            state: self.state.public(),
        }
    }
}

fn query_crtc_mode(
    client: &CastKmsClient,
    crtc_id: NonZeroU32,
) -> Result<(NonZeroU32, NonZeroU32, NonZeroU32), CaptureError> {
    let mut crtc = DrmModeCrtc {
        crtc_id: crtc_id.get(),
        ..DrmModeCrtc::default()
    };
    // SAFETY: `crtc` has the standard DRM UAPI layout and remains writable
    // for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_mode_getcrtc(client.as_raw_fd(), &mut crtc) }
        .map_err(CaptureError::QueryCrtc)?;
    if crtc.crtc_id != crtc_id.get() {
        return Err(CaptureError::InvalidKernelResult(
            "CRTC query changed the CRTC ID",
        ));
    }
    if crtc.mode_valid != 1 {
        return Err(CaptureError::InvalidCrtcMode("CRTC has no active mode"));
    }
    let width = NonZeroU32::new(u32::from(crtc.mode.hdisplay))
        .ok_or(CaptureError::InvalidCrtcMode("zero horizontal display"))?;
    let height = NonZeroU32::new(u32::from(crtc.mode.vdisplay))
        .ok_or(CaptureError::InvalidCrtcMode("zero vertical display"))?;
    let refresh_hz = NonZeroU32::new(crtc.mode.vrefresh)
        .ok_or(CaptureError::InvalidCrtcMode("zero refresh rate"))?;
    Ok((width, height, refresh_hz))
}

fn validate_dumb_allocation(
    stream: CaptureStreamInfo,
    dumb: &DrmModeCreateDumb,
) -> Result<NonZeroU32, CaptureError> {
    if dumb.width != stream.width.get()
        || dumb.height != stream.height.get()
        || dumb.bpp != 32
        || dumb.flags != 0
    {
        return Err(CaptureError::InvalidKernelResult(
            "dumb-buffer creation modified immutable fields",
        ));
    }
    let handle = NonZeroU32::new(dumb.handle)
        .ok_or(CaptureError::InvalidBufferAllocation("zero GEM handle"))?;
    let pitch =
        NonZeroU32::new(dumb.pitch).ok_or(CaptureError::InvalidBufferAllocation("zero pitch"))?;
    let size =
        NonZeroU64::new(dumb.size).ok_or(CaptureError::InvalidBufferAllocation("zero size"))?;
    let minimum_pitch = stream
        .width
        .get()
        .checked_mul(4)
        .ok_or(CaptureError::InvalidBufferAllocation("pitch overflow"))?;
    if pitch.get() < minimum_pitch {
        return Err(CaptureError::InvalidBufferAllocation(
            "pitch is smaller than one XRGB row",
        ));
    }
    let minimum_size = u64::from(pitch.get())
        .checked_mul(u64::from(stream.height.get()))
        .ok_or(CaptureError::InvalidBufferAllocation("size overflow"))?;
    if size.get() < minimum_size {
        return Err(CaptureError::InvalidBufferAllocation(
            "size is smaller than pitch times height",
        ));
    }
    if size.get() > MAX_CAPTURE_BUFFER_BYTES {
        return Err(CaptureError::InvalidBufferAllocation(
            "size exceeds the client allocation bound",
        ));
    }
    Ok(handle)
}

fn validate_framebuffer_result(
    framebuffer: &DrmModeFbCmd2,
    layout: CaptureBufferLayout,
    gem_handle: NonZeroU32,
) -> Result<NonZeroU32, CaptureError> {
    if framebuffer.width != layout.width.get()
        || framebuffer.height != layout.height.get()
        || framebuffer.pixel_format != layout.format
        || framebuffer.flags != 0
        || framebuffer.handles != [gem_handle.get(), 0, 0, 0]
        || framebuffer.pitches != [layout.pitch.get(), 0, 0, 0]
        || framebuffer.offsets != [0; 4]
        || framebuffer.modifier != [layout.modifier, 0, 0, 0]
    {
        return Err(CaptureError::InvalidKernelResult(
            "framebuffer creation modified immutable fields",
        ));
    }
    NonZeroU32::new(framebuffer.fb_id)
        .ok_or(CaptureError::InvalidBufferAllocation("zero framebuffer ID"))
}

fn owned_fd_from_kernel(
    raw_fd: i32,
    immutable_fields_valid: bool,
    operation: &'static str,
) -> Result<OwnedFd, CaptureError> {
    if raw_fd < 0 {
        return Err(CaptureError::InvalidKernelResult(operation));
    }
    // SAFETY: a successful descriptor-producing ioctl returned a new owned
    // descriptor. Construct it before any remaining validation so failure
    // paths still close it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    if !immutable_fields_valid {
        return Err(CaptureError::InvalidKernelResult(operation));
    }
    Ok(fd)
}

fn export_syncobj_fd(
    fd: std::os::fd::RawFd,
    handle: NonZeroU32,
    map_error: fn(Errno) -> CaptureError,
) -> Result<OwnedFd, CaptureError> {
    let mut args = DrmSyncobjHandle {
        handle: handle.get(),
        flags: DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_NONE,
        fd: -1,
        ..DrmSyncobjHandle::default()
    };
    // SAFETY: `args` has the standard DRM UAPI layout, names one live
    // client-owned syncobj, and remains writable for the synchronous ioctl.
    unsafe { drm_ioctl_syncobj_handle_to_fd(fd, &mut args) }.map_err(map_error)?;
    owned_fd_from_kernel(
        args.fd,
        args.handle == handle.get()
            && args.flags == DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_NONE
            && args.pad == 0
            && args.point == 0,
        "syncobj fd export",
    )
}

fn create_syncobj_pair(fd: std::os::fd::RawFd) -> Result<OwnedSyncobjPair, CaptureError> {
    let ready_handle = create_syncobj(fd, CaptureError::CreateReadySyncobj)?;
    let reuse_handle = match create_syncobj(fd, CaptureError::CreateReuseSyncobj) {
        Ok(handle) => handle,
        Err(error) => {
            destroy_syncobj_best_effort(fd, ready_handle);
            return Err(error);
        }
    };
    if ready_handle == reuse_handle {
        destroy_syncobj_best_effort(fd, ready_handle);
        return Err(CaptureError::InvalidKernelResult(
            "syncobj creation returned duplicate live handles",
        ));
    }
    Ok(OwnedSyncobjPair {
        ready_handle,
        reuse_handle,
    })
}

fn create_syncobj(
    fd: std::os::fd::RawFd,
    map_error: fn(Errno) -> CaptureError,
) -> Result<NonZeroU32, CaptureError> {
    let mut args = DrmSyncobjCreate::default();
    // SAFETY: `args` has the standard DRM UAPI layout and remains writable for
    // the duration of the synchronous ioctl.
    unsafe { drm_ioctl_syncobj_create(fd, &mut args) }.map_err(map_error)?;
    let handle =
        NonZeroU32::new(args.handle).ok_or(CaptureError::ZeroKernelIdentifier("syncobj handle"))?;
    if args.flags != 0 {
        destroy_syncobj_best_effort(fd, handle);
        return Err(CaptureError::InvalidKernelResult(
            "syncobj creation modified immutable flags",
        ));
    }
    Ok(handle)
}

fn destroy_syncobj_pair(
    fd: std::os::fd::RawFd,
    pair: OwnedSyncobjPair,
) -> Result<(), CaptureError> {
    let mut cleanup_error = destroy_syncobj(fd, pair.ready_handle)
        .err()
        .map(CaptureError::DestroyReadySyncobj);
    if let Err(error) = destroy_syncobj(fd, pair.reuse_handle) {
        cleanup_error.get_or_insert(CaptureError::DestroyReuseSyncobj(error));
    }
    match cleanup_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn destroy_syncobj(fd: std::os::fd::RawFd, handle: NonZeroU32) -> Result<(), Errno> {
    let mut args = DrmSyncobjDestroy {
        handle: handle.get(),
        ..DrmSyncobjDestroy::default()
    };
    // SAFETY: `args` has the standard DRM UAPI layout and names a client-owned
    // live handle for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_syncobj_destroy(fd, &mut args) }?;
    Ok(())
}

fn destroy_syncobj_best_effort(fd: std::os::fd::RawFd, handle: NonZeroU32) {
    let _ = destroy_syncobj(fd, handle);
}

fn destroy_syncobj_pair_best_effort(
    fd: std::os::fd::RawFd,
    synchronization: TrackedSynchronization,
) {
    if let TrackedSynchronization::Explicit(pair) = synchronization {
        let _ = destroy_syncobj_pair(fd, pair);
    }
}

fn signal_syncobj_point(
    fd: std::os::fd::RawFd,
    handle: NonZeroU32,
    point: NonZeroU64,
) -> Result<(), CaptureError> {
    let handles = [handle.get()];
    let points = [point.get()];
    let mut args = DrmSyncobjTimelineArray {
        handles: u64::try_from(handles.as_ptr() as usize)
            .expect("supported Rust targets have pointers no wider than 64 bits"),
        points: u64::try_from(points.as_ptr() as usize)
            .expect("supported Rust targets have pointers no wider than 64 bits"),
        count_handles: 1,
        ..DrmSyncobjTimelineArray::default()
    };
    // SAFETY: `args` points to one live syncobj handle and timeline point with
    // standard DRM UAPI layouts for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_syncobj_timeline_signal(fd, &mut args) }
        .map_err(CaptureError::SignalReusePoint)?;
    if args.handles
        != u64::try_from(handles.as_ptr() as usize)
            .expect("supported Rust targets have pointers no wider than 64 bits")
        || args.points
            != u64::try_from(points.as_ptr() as usize)
                .expect("supported Rust targets have pointers no wider than 64 bits")
        || args.count_handles != 1
        || args.flags != 0
        || handles != [handle.get()]
        || points != [point.get()]
    {
        return Err(CaptureError::InvalidKernelResult(
            "syncobj timeline signal modified immutable fields",
        ));
    }
    Ok(())
}

fn cleanup_tracked_buffer_resources(
    fd: std::os::fd::RawFd,
    buffer: TrackedBuffer,
) -> Result<(), CaptureError> {
    let TrackedBuffer {
        framebuffer_id,
        synchronization,
        owned_framebuffer,
        ..
    } = buffer;
    let mut cleanup_error = match synchronization {
        TrackedSynchronization::Implicit => None,
        TrackedSynchronization::Explicit(pair) => destroy_syncobj_pair(fd, pair).err(),
    };
    if let Some(owned) = owned_framebuffer {
        if let Err(error) = cleanup_owned_framebuffer(fd, framebuffer_id, owned) {
            cleanup_error.get_or_insert(error);
        }
    }
    match cleanup_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn cleanup_owned_framebuffer(
    fd: std::os::fd::RawFd,
    framebuffer_id: NonZeroU32,
    owned: OwnedFramebuffer,
) -> Result<(), CaptureError> {
    let OwnedFramebuffer {
        gem_handle,
        dma_buf,
        ..
    } = owned;
    drop(dma_buf);

    let mut cleanup_error = None;
    let mut raw_framebuffer_id = framebuffer_id.get();
    // SAFETY: the framebuffer ID is passed with the standard DRM UAPI type and
    // remains writable for the duration of the synchronous ioctl.
    if let Err(error) = unsafe { drm_ioctl_mode_rmfb(fd, &mut raw_framebuffer_id) } {
        cleanup_error = Some(CaptureError::RemoveFramebuffer(error));
    }
    let mut destroy = DrmModeDestroyDumb {
        handle: gem_handle.get(),
    };
    // SAFETY: `destroy` has the standard DRM UAPI layout and remains writable
    // for the duration of the synchronous ioctl.
    if let Err(error) = unsafe { drm_ioctl_mode_destroy_dumb(fd, &mut destroy) } {
        cleanup_error.get_or_insert(CaptureError::DestroyDumbBuffer(error));
    }
    match cleanup_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn cleanup_owned_framebuffer_best_effort(
    fd: std::os::fd::RawFd,
    framebuffer_id: NonZeroU32,
    owned: OwnedFramebuffer,
) {
    let _ = cleanup_owned_framebuffer(fd, framebuffer_id, owned);
}

fn remove_framebuffer_best_effort(fd: std::os::fd::RawFd, framebuffer_id: u32) {
    if framebuffer_id == 0 {
        return;
    }
    let mut framebuffer_id = framebuffer_id;
    // SAFETY: best-effort unwind of the framebuffer ID just returned by the
    // kernel using the standard DRM UAPI type.
    let _ = unsafe { drm_ioctl_mode_rmfb(fd, &mut framebuffer_id) };
}

fn destroy_dumb_best_effort(fd: std::os::fd::RawFd, gem_handle: u32) {
    if gem_handle == 0 {
        return;
    }
    let mut destroy = DrmModeDestroyDumb { handle: gem_handle };
    // SAFETY: best-effort unwind of the GEM handle just returned by the kernel
    // using the standard DRM UAPI layout.
    let _ = unsafe { drm_ioctl_mode_destroy_dumb(fd, &mut destroy) };
}

fn stop_stream_best_effort(client: &CastKmsClient, stream_id: u32) {
    if stream_id == 0 {
        return;
    }
    let args = DrmCastkmsCaptureStop {
        stream_id,
        ..DrmCastkmsCaptureStop::default()
    };
    // SAFETY: this is best-effort unwind of a stream identifier just returned
    // by START; `args` has the checked-in UAPI layout.
    let _ = unsafe { drm_ioctl_castkms_capture_stop(client.as_raw_fd(), &args) };
}

fn query_capabilities_ioctl(
    client: &CastKmsClient,
    crtc_id: NonZeroU32,
    formats: &mut [DrmCastkmsCaptureFormat],
) -> Result<DrmCastkmsCaptureQueryCaps, CaptureError> {
    let mut query = DrmCastkmsCaptureQueryCaps {
        crtc_id: crtc_id.get(),
        format_count: formats.len() as u32,
        formats_ptr: if formats.is_empty() {
            0
        } else {
            u64::try_from(formats.as_mut_ptr() as usize)
                .expect("supported Rust targets have pointers no wider than 64 bits")
        },
        ..DrmCastkmsCaptureQueryCaps::default()
    };
    // SAFETY: `query` and the optional format slice have checked-in UAPI
    // layouts and remain writable for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_castkms_capture_query_caps(client.as_raw_fd(), &mut query) }
        .map_err(CaptureError::QueryCapabilities)?;
    Ok(query)
}

fn validate_capability_header(
    client: &CastKmsClient,
    crtc_id: NonZeroU32,
    query: &DrmCastkmsCaptureQueryCaps,
) -> Result<(), CaptureError> {
    if query.uapi_major != u32::from(CAPTURE_UAPI_MAJOR)
        || query.uapi_minor < u32::from(CAPTURE_UAPI_MINOR)
        || query.uapi_major != u32::from(client.lease.capture_uapi_major())
        || query.uapi_minor < u32::from(client.lease.capture_uapi_minor())
    {
        return Err(CaptureError::InvalidCapabilities("UAPI version"));
    }
    if query.crtc_id != crtc_id.get() {
        return Err(CaptureError::InvalidCapabilities("CRTC ID"));
    }
    if query.reserved != 0 {
        return Err(CaptureError::InvalidCapabilities("reserved field"));
    }
    if query.flags & CAPTURE_CAP_GRANT_FD == 0 {
        return Err(CaptureError::InvalidCapabilities("grant-fd capability"));
    }
    if query.flags & CAPTURE_CAP_GRANT_CONTROL_FD == 0 {
        return Err(CaptureError::InvalidCapabilities(
            "grant-control-fd capability",
        ));
    }
    if query.flags & (CAPTURE_CAP_SYNCOBJ_TIMELINE | CAPTURE_CAP_IMPLICIT_SYNC) == 0 {
        return Err(CaptureError::InvalidCapabilities("synchronization modes"));
    }
    if query.max_registered_buffers == 0 {
        return Err(CaptureError::InvalidCapabilities("zero buffer limit"));
    }
    Ok(())
}

fn require_buffer_state(
    buffer: &TrackedBuffer,
    expected: CaptureBufferState,
) -> Result<(), CaptureError> {
    let actual = buffer.state.public();
    if actual == expected {
        Ok(())
    } else {
        Err(CaptureError::InvalidBufferState {
            buffer_id: buffer.id.get(),
            expected,
            actual,
        })
    }
}

fn queue_points(buffer: &TrackedBuffer) -> Result<QueuePoints, CaptureError> {
    match &buffer.synchronization {
        TrackedSynchronization::Implicit => Ok(QueuePoints {
            ready: None,
            reuse: None,
            next_ready: None,
        }),
        TrackedSynchronization::Explicit(_) => {
            let ready_point =
                NonZeroU64::new(buffer.next_ready_point).ok_or(CaptureError::ReadyPointOverflow)?;
            let next_ready_point = buffer
                .next_ready_point
                .checked_add(1)
                .ok_or(CaptureError::ReadyPointOverflow)?;
            Ok(QueuePoints {
                ready: Some(ready_point),
                reuse: NonZeroU64::new(buffer.last_release_point),
                next_ready: Some(next_ready_point),
            })
        }
    }
}

fn next_reuse_point(buffer: &TrackedBuffer) -> Result<Option<NonZeroU64>, CaptureError> {
    match &buffer.synchronization {
        TrackedSynchronization::Implicit => Ok(None),
        TrackedSynchronization::Explicit(_) => buffer
            .last_release_point
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Some)
            .ok_or(CaptureError::ReusePointOverflow),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    use castkms_sys::{CAPTURE_CAP_DMA_BUF_IMPORT, DISPLAY_V1_RIGHTS, GRANT_STATE_ACTIVE};

    use crate::grant::{GrantLease, GrantMetadata};

    const STREAM_ID: u32 = 7;
    const BUFFER_ID: u32 = 3;
    const FRAMEBUFFER_ID: u32 = 91;
    const CRTC_ID: u32 = 41;
    const MODE_GENERATION: u64 = 12;
    const USER_DATA: u64 = 0x1122_3344_5566_7788;

    #[test]
    fn implicit_buffer_moves_through_the_complete_ownership_cycle() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(MODE_GENERATION, USER_DATA, 0, 0);

        client.record_capture_frame(frame).unwrap();
        assert_eq!(buffer_state(&client), CaptureBufferState::Completed);
        let completion = client.take_capture_completion(ready_for(queue)).unwrap();
        assert_eq!(completion.frame, frame);
        assert_eq!(completion.queue, queue);
        assert_eq!(buffer_state(&client), CaptureBufferState::ConsumerOwned);
        let release = client
            .release_capture_buffer(nonzero32(STREAM_ID), nonzero32(BUFFER_ID))
            .unwrap();
        assert_eq!(release.reuse_point, None);
        assert_eq!(buffer_state(&client), CaptureBufferState::Idle);
    }

    #[test]
    fn explicit_release_points_are_client_owned_and_strictly_increase() {
        let synchronization = explicit_sync();
        let queue = test_queue(synchronization);
        let completion = CaptureCompletion {
            queue,
            frame: test_frame(MODE_GENERATION, USER_DATA, 0, 0),
        };
        let mut buffer = test_buffer(
            synchronization,
            TrackedBufferState::ConsumerOwned(completion),
        );
        assert_eq!(next_reuse_point(&buffer).unwrap(), Some(nonzero64(1)));
        buffer.last_release_point = 1;
        assert_eq!(next_reuse_point(&buffer).unwrap(), Some(nonzero64(2)));
        buffer.last_release_point = u64::MAX;
        assert!(matches!(
            next_reuse_point(&buffer),
            Err(CaptureError::ReusePointOverflow)
        ));
    }

    #[test]
    fn implicit_buffers_have_no_release_timeline_point() {
        let buffer = test_buffer(CaptureSynchronization::Implicit, TrackedBufferState::Idle);
        assert_eq!(next_reuse_point(&buffer).unwrap(), None);
    }

    #[test]
    fn one_outstanding_capture_request_fills_the_kernel_queue() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let stream = client.capture.active.as_mut().unwrap();

        assert!(!stream.has_capture_queue_capacity());
    }

    #[test]
    fn frame_correlation_rejects_wrong_identity_and_state() {
        let queue = test_queue(CaptureSynchronization::Implicit);

        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let mut frame = test_frame(MODE_GENERATION, USER_DATA, 0, 0);
        frame.stream_id += 1;
        assert!(matches!(
            client.record_capture_frame(frame),
            Err(CaptureProtocolError::UnknownStream(_))
        ));

        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let mut frame = test_frame(MODE_GENERATION, USER_DATA, 0, 0);
        frame.buffer_id += 1;
        assert!(matches!(
            client.record_capture_frame(frame),
            Err(CaptureProtocolError::UnknownBuffer { .. })
        ));

        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(MODE_GENERATION, USER_DATA + 1, 0, 0);
        assert_eq!(
            client.record_capture_frame(frame).unwrap_err(),
            CaptureProtocolError::UserDataMismatch {
                expected: USER_DATA,
                actual: USER_DATA + 1,
            }
        );

        let mut client = test_client(CaptureSynchronization::Implicit, TrackedBufferState::Idle);
        let frame = test_frame(MODE_GENERATION, USER_DATA, 0, 0);
        assert_eq!(
            client.record_capture_frame(frame).unwrap_err(),
            CaptureProtocolError::UnexpectedBufferState {
                buffer_id: BUFFER_ID,
                actual: CaptureBufferState::Idle,
            }
        );
    }

    #[test]
    fn successful_frames_require_the_stream_generation() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(MODE_GENERATION + 1, USER_DATA, 0, 0);
        assert_eq!(
            client.record_capture_frame(frame).unwrap_err(),
            CaptureProtocolError::GenerationMismatch {
                expected: MODE_GENERATION,
                actual: MODE_GENERATION + 1,
            }
        );

        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(0, USER_DATA, -116, CAPTURE_FRAME_MODE_CHANGED);
        assert_eq!(
            client.record_capture_frame(frame).unwrap_err(),
            CaptureProtocolError::ZeroModeGeneration
        );
    }

    #[test]
    fn mode_change_completes_the_buffer_and_marks_the_stream_stale() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(
            MODE_GENERATION + 1,
            USER_DATA,
            -(Errno::ESTALE as i32),
            CAPTURE_FRAME_MODE_CHANGED,
        );
        client.record_capture_frame(frame).unwrap();
        assert!(client.active_capture_requires_restart());
        assert_eq!(buffer_state(&client), CaptureBufferState::Completed);
        let completion = client.take_capture_completion(ready_for(queue)).unwrap();
        assert!(completion.mode_changed());

        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(MODE_GENERATION + 1, USER_DATA, -11, 0);
        client.record_capture_frame(frame).unwrap();
        assert!(!client.active_capture_requires_restart());
    }

    #[test]
    fn mode_change_requires_estale_and_a_new_generation() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(
            MODE_GENERATION + 1,
            USER_DATA,
            0,
            CAPTURE_FRAME_MODE_CHANGED,
        );
        assert_eq!(
            client.record_capture_frame(frame).unwrap_err(),
            CaptureProtocolError::InvalidModeChangeStatus { actual: 0 }
        );
        assert_eq!(buffer_state(&client), CaptureBufferState::Queued);
        assert!(!client.active_capture_requires_restart());

        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        let frame = test_frame(
            MODE_GENERATION,
            USER_DATA,
            -(Errno::ESTALE as i32),
            CAPTURE_FRAME_MODE_CHANGED,
        );
        assert_eq!(
            client.record_capture_frame(frame).unwrap_err(),
            CaptureProtocolError::UnchangedModeGeneration(MODE_GENERATION)
        );
        assert_eq!(buffer_state(&client), CaptureBufferState::Queued);
        assert!(!client.active_capture_requires_restart());
    }

    #[test]
    fn synchronous_estale_marks_only_the_named_active_stream() {
        let mut client = test_client(CaptureSynchronization::Implicit, TrackedBufferState::Idle);
        assert!(!client.active_capture_requires_restart());
        client.capture.mark_active_stale(nonzero32(STREAM_ID + 1));
        assert!(!client.active_capture_requires_restart());
        client.capture.mark_active_stale(nonzero32(STREAM_ID));
        assert!(client.active_capture_requires_restart());
    }

    #[test]
    fn grant_state_evidence_is_semantically_validated() {
        for (state, status) in [
            (GrantState::Pending, -(Errno::ENOLINK as i32)),
            (GrantState::Active, 0),
            (GrantState::SuspendedNoMaster, -(Errno::EAGAIN as i32)),
            (GrantState::SuspendedOtherMaster, -(Errno::EAGAIN as i32)),
            (GrantState::SuspendedForeignContent, -(Errno::ESTALE as i32)),
        ] {
            validate_grant_state_evidence(
                1,
                GrantStateEvidence::Event(test_grant_state_event(1, state, status)),
            )
            .unwrap();
        }

        assert!(matches!(
            validate_grant_state_evidence(
                1,
                GrantStateEvidence::Event(test_grant_state_event(2, GrantState::Active, 0))
            ),
            Err(CaptureError::ForeignGrantStateEvent {
                expected: 1,
                actual: 2
            })
        ));
        assert!(matches!(
            validate_grant_state_evidence(
                1,
                GrantStateEvidence::Event(test_grant_state_event(1, GrantState::Pending, 0))
            ),
            Err(CaptureError::InvalidGrantStateEventStatus { .. })
        ));
        assert!(matches!(
            validate_grant_state_evidence(
                1,
                GrantStateEvidence::Event(test_grant_state_event(
                    1,
                    GrantState::Revoked,
                    -(Errno::EKEYREVOKED as i32)
                ))
            ),
            Err(CaptureError::TerminalGrantStateEvent)
        ));
        for status in [
            Errno::ENOENT,
            Errno::EAGAIN,
            Errno::ESTALE,
            Errno::ENOLINK,
            Errno::ENOTCONN,
        ] {
            validate_grant_state_evidence(1, GrantStateEvidence::CaptureInvalidated(status))
                .unwrap();
        }
        assert!(matches!(
            validate_grant_state_evidence(1, GrantStateEvidence::CaptureInvalidated(Errno::EINVAL)),
            Err(CaptureError::InvalidCaptureInvalidation(Errno::EINVAL))
        ));
    }

    #[test]
    fn reconciliation_preserves_only_proven_active_streams() {
        assert!(!capture_restart_required(
            GrantState::Active,
            GrantStateEvidence::Query
        ));
        assert!(!capture_restart_required(
            GrantState::Active,
            GrantStateEvidence::Event(test_grant_state_event(1, GrantState::Active, 0))
        ));
        assert!(capture_restart_required(
            GrantState::Active,
            GrantStateEvidence::Event(test_grant_state_event(
                1,
                GrantState::SuspendedNoMaster,
                -(Errno::EAGAIN as i32)
            ))
        ));
        assert!(capture_restart_required(
            GrantState::Active,
            GrantStateEvidence::CaptureInvalidated(Errno::ENOENT)
        ));
        assert!(capture_restart_required(
            GrantState::Pending,
            GrantStateEvidence::Query
        ));
    }

    #[test]
    fn retired_stream_waits_for_completion_and_consumer_release() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        assert_eq!(client.capture.retire_active(), 1);
        assert!(client.capture.active.is_none());
        assert!(matches!(
            client.finish_retired_capture(nonzero32(STREAM_ID)),
            Err(CaptureError::RetiredStreamBusy)
        ));

        let frame = test_frame(MODE_GENERATION, USER_DATA, -125, 0);
        client.record_capture_frame(frame).unwrap();
        client.take_capture_completion(ready_for(queue)).unwrap();
        client
            .release_capture_buffer(nonzero32(STREAM_ID), nonzero32(BUFFER_ID))
            .unwrap();
        let retired = client.finish_retired_capture(nonzero32(STREAM_ID)).unwrap();
        assert_eq!(retired.stream.stream_id.get(), STREAM_ID);
        assert_eq!(retired.buffers.len(), 1);
        assert_eq!(retired.buffers[0].state, CaptureBufferState::Idle);
        assert!(client.capture.retired.is_none());
    }

    #[test]
    fn retired_stream_accepts_an_exact_silent_cancellation() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Queued(queue),
        );
        assert_eq!(client.capture.retire_active(), 1);

        let mut wrong_queue = queue;
        wrong_queue.user_data = nonzero64(USER_DATA + 1);
        assert!(matches!(
            client.retire_cancelled_capture_buffer(wrong_queue),
            Err(CaptureError::CancelledQueueMismatch)
        ));
        client.retire_cancelled_capture_buffer(queue).unwrap();

        let retired = client.finish_retired_capture(nonzero32(STREAM_ID)).unwrap();
        assert_eq!(retired.buffers.len(), 1);
        assert_eq!(retired.buffers[0].state, CaptureBufferState::Idle);
    }

    #[test]
    fn explicit_queue_points_start_at_one_and_are_bounded() {
        let mut buffer = test_buffer(explicit_sync(), TrackedBufferState::Idle);
        assert_eq!(
            queue_points(&buffer).unwrap(),
            QueuePoints {
                ready: Some(nonzero64(1)),
                reuse: None,
                next_ready: Some(2),
            }
        );
        buffer.next_ready_point = 2;
        buffer.last_release_point = 1;
        assert_eq!(
            queue_points(&buffer).unwrap(),
            QueuePoints {
                ready: Some(nonzero64(2)),
                reuse: Some(nonzero64(1)),
                next_ready: Some(3),
            }
        );
        buffer.next_ready_point = u64::MAX;
        assert!(matches!(
            queue_points(&buffer),
            Err(CaptureError::ReadyPointOverflow)
        ));

        let buffer = test_buffer(CaptureSynchronization::Implicit, TrackedBufferState::Idle);
        assert_eq!(
            queue_points(&buffer).unwrap(),
            QueuePoints {
                ready: None,
                reuse: None,
                next_ready: None,
            }
        );
    }

    #[test]
    fn capability_header_requires_the_grant_descriptor_pair_and_a_sync_mode() {
        let client = test_client(CaptureSynchronization::Implicit, TrackedBufferState::Idle);
        let mut query = DrmCastkmsCaptureQueryCaps {
            uapi_major: u32::from(CAPTURE_UAPI_MAJOR),
            uapi_minor: u32::from(CAPTURE_UAPI_MINOR),
            crtc_id: CRTC_ID,
            format_count: 1,
            flags: CAPTURE_CAP_GRANT_FD | CAPTURE_CAP_GRANT_CONTROL_FD | CAPTURE_CAP_IMPLICIT_SYNC,
            max_registered_buffers: 8,
            ..DrmCastkmsCaptureQueryCaps::default()
        };
        validate_capability_header(&client, nonzero32(CRTC_ID), &query).unwrap();

        query.flags = CAPTURE_CAP_IMPLICIT_SYNC;
        assert!(matches!(
            validate_capability_header(&client, nonzero32(CRTC_ID), &query),
            Err(CaptureError::InvalidCapabilities("grant-fd capability"))
        ));
        query.flags = CAPTURE_CAP_GRANT_FD | CAPTURE_CAP_IMPLICIT_SYNC;
        assert!(matches!(
            validate_capability_header(&client, nonzero32(CRTC_ID), &query),
            Err(CaptureError::InvalidCapabilities(
                "grant-control-fd capability"
            ))
        ));
        query.flags = CAPTURE_CAP_GRANT_FD | CAPTURE_CAP_GRANT_CONTROL_FD;
        assert!(matches!(
            validate_capability_header(&client, nonzero32(CRTC_ID), &query),
            Err(CaptureError::InvalidCapabilities("synchronization modes"))
        ));
    }

    #[test]
    fn userspace_buffer_tracking_has_a_fixed_upper_bound() {
        let mut capabilities = test_capabilities();
        capabilities.max_registered_buffers = u32::MAX;
        assert_eq!(
            capabilities.max_registered_buffers(),
            MAX_TRACKED_CAPTURE_BUFFERS
        );
    }

    #[test]
    fn dumb_buffer_results_are_mode_matched_and_bounded() {
        let stream = test_stream_info();
        let valid = DrmModeCreateDumb {
            width: stream.width.get(),
            height: stream.height.get(),
            bpp: 32,
            handle: 17,
            pitch: stream.width.get() * 4,
            size: u64::from(stream.width.get() * 4) * u64::from(stream.height.get()),
            ..DrmModeCreateDumb::default()
        };
        assert_eq!(
            validate_dumb_allocation(stream, &valid).unwrap(),
            nonzero32(17)
        );

        let mut invalid = valid;
        invalid.width += 1;
        assert!(matches!(
            validate_dumb_allocation(stream, &invalid),
            Err(CaptureError::InvalidKernelResult(_))
        ));
        invalid = valid;
        invalid.pitch -= 1;
        assert!(matches!(
            validate_dumb_allocation(stream, &invalid),
            Err(CaptureError::InvalidBufferAllocation(
                "pitch is smaller than one XRGB row"
            ))
        ));
        invalid = valid;
        invalid.size = MAX_CAPTURE_BUFFER_BYTES + 1;
        assert!(matches!(
            validate_dumb_allocation(stream, &invalid),
            Err(CaptureError::InvalidBufferAllocation(
                "size exceeds the client allocation bound"
            ))
        ));
    }

    #[test]
    fn framebuffer_results_preserve_the_selected_layout() {
        let layout = CaptureBufferLayout {
            width: nonzero32(1920),
            height: nonzero32(1080),
            format: DRM_FORMAT_XRGB8888,
            modifier: DRM_FORMAT_MOD_LINEAR,
            pitch: nonzero32(7680),
            size: nonzero64(8_294_400),
        };
        let handle = nonzero32(17);
        let mut framebuffer = DrmModeFbCmd2 {
            fb_id: 23,
            width: layout.width.get(),
            height: layout.height.get(),
            pixel_format: layout.format,
            handles: [handle.get(), 0, 0, 0],
            pitches: [layout.pitch.get(), 0, 0, 0],
            modifier: [layout.modifier, 0, 0, 0],
            ..DrmModeFbCmd2::default()
        };
        assert_eq!(
            validate_framebuffer_result(&framebuffer, layout, handle).unwrap(),
            nonzero32(23)
        );
        framebuffer.offsets[0] = 1;
        assert!(matches!(
            validate_framebuffer_result(&framebuffer, layout, handle),
            Err(CaptureError::InvalidKernelResult(_))
        ));
    }

    #[test]
    fn ready_evidence_must_match_the_reported_queue() {
        let queue = test_queue(CaptureSynchronization::Implicit);
        let completion = CaptureCompletion {
            queue,
            frame: test_frame(MODE_GENERATION, USER_DATA, 0, 0),
        };
        let mut client = test_client(
            CaptureSynchronization::Implicit,
            TrackedBufferState::Completed(completion),
        );
        let ready = CaptureReady {
            user_data: nonzero64(USER_DATA + 1),
            ..ready_for(queue)
        };
        assert!(matches!(
            client.take_capture_completion(ready),
            Err(CaptureError::ReadyUserDataMismatch {
                expected: USER_DATA,
                actual
            }) if actual == USER_DATA + 1
        ));
        assert_eq!(buffer_state(&client), CaptureBufferState::Completed);

        let queue = test_queue(explicit_sync());
        let completion = CaptureCompletion {
            queue,
            frame: test_frame(MODE_GENERATION, USER_DATA, 0, 0),
        };
        let mut client = test_client(explicit_sync(), TrackedBufferState::Completed(completion));
        let ready = CaptureReady {
            ready_point: Some(nonzero64(2)),
            ..ready_for(queue)
        };
        assert!(matches!(
            client.take_capture_completion(ready),
            Err(CaptureError::ReadyPointMismatch {
                expected: Some(expected),
                actual: Some(actual),
            }) if expected.get() == 1 && actual.get() == 2
        ));
        assert_eq!(buffer_state(&client), CaptureBufferState::Completed);
    }

    #[test]
    fn implicit_fence_wait_uses_tokio_readiness_and_preserves_identity() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.write_all(&[1]).unwrap();
        let fence = ImplicitCaptureFence {
            stream_id: nonzero32(STREAM_ID),
            buffer_id: nonzero32(BUFFER_ID),
            user_data: nonzero64(USER_DATA),
            fence: OwnedFd::from(reader),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let ready = runtime.block_on(fence.wait()).unwrap();
        assert_eq!(ready.stream_id(), nonzero32(STREAM_ID));
        assert_eq!(ready.buffer_id(), nonzero32(BUFFER_ID));
        assert_eq!(ready.user_data(), nonzero64(USER_DATA));
        assert_eq!(ready.ready_point(), None);
    }

    #[test]
    fn explicit_fence_wait_uses_tokio_eventfd_and_preserves_identity() {
        let event = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK).unwrap();
        event.arm().unwrap();
        let fence = ExplicitCaptureFence {
            stream_id: nonzero32(STREAM_ID),
            buffer_id: nonzero32(BUFFER_ID),
            user_data: nonzero64(USER_DATA),
            ready_point: nonzero64(4),
            event,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let ready = runtime.block_on(fence.wait()).unwrap();
        assert_eq!(ready.stream_id(), nonzero32(STREAM_ID));
        assert_eq!(ready.buffer_id(), nonzero32(BUFFER_ID));
        assert_eq!(ready.user_data(), nonzero64(USER_DATA));
        assert_eq!(ready.ready_point(), Some(nonzero64(4)));
    }

    #[test]
    fn explicit_completion_can_be_delegated_without_waiting() {
        let queue = test_queue(explicit_sync());
        let completion = CaptureCompletion {
            queue,
            frame: test_frame(MODE_GENERATION, USER_DATA, 0, 0),
        };
        let mut client = test_client(explicit_sync(), TrackedBufferState::Completed(completion));
        let event = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK).unwrap();
        let fence = ExplicitCaptureFence {
            stream_id: queue.stream_id,
            buffer_id: queue.buffer_id,
            user_data: queue.user_data,
            ready_point: queue.ready_point.unwrap(),
            event,
        };

        let delegated = client.delegate_explicit_capture_completion(fence).unwrap();
        assert_eq!(delegated, completion);
        assert_eq!(buffer_state(&client), CaptureBufferState::ConsumerOwned);
    }

    fn test_client(
        synchronization: CaptureSynchronization,
        buffer_state: TrackedBufferState,
    ) -> CastKmsClient {
        let (holder, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let holder = OwnedFd::from(holder);
        let lease = GrantLease::new_unchecked(
            holder,
            GrantMetadata {
                grant_id: 1,
                connector_id: 43,
                output_index: 3,
                rights: DISPLAY_V1_RIGHTS,
                flags: 0,
                initial_state: GRANT_STATE_ACTIVE,
                capture_uapi_major: CAPTURE_UAPI_MAJOR,
                capture_uapi_minor: CAPTURE_UAPI_MINOR,
            },
        );
        CastKmsClient {
            lease,
            capture: CaptureTracker {
                active: Some(TrackedStream {
                    info: test_stream_info(),
                    capabilities: test_capabilities(),
                    stale: false,
                    buffers: vec![test_buffer(synchronization, buffer_state)],
                }),
                retired: None,
            },
            cec: crate::castkms::cec::CecTracker::default(),
        }
    }

    fn test_capabilities() -> CaptureCapabilities {
        CaptureCapabilities {
            uapi_major: CAPTURE_UAPI_MAJOR,
            uapi_minor: CAPTURE_UAPI_MINOR,
            crtc_id: nonzero32(CRTC_ID),
            flags: CAPTURE_CAP_GRANT_FD
                | CAPTURE_CAP_GRANT_CONTROL_FD
                | CAPTURE_CAP_IMPLICIT_SYNC
                | CAPTURE_CAP_SYNCOBJ_TIMELINE
                | CAPTURE_CAP_DMA_BUF_IMPORT,
            formats: vec![CaptureFormat {
                format: 0x3432_5258,
                modifier: 0,
            }]
            .into_boxed_slice(),
            max_registered_buffers: 8,
        }
    }

    fn test_stream_info() -> CaptureStreamInfo {
        CaptureStreamInfo {
            stream_id: nonzero32(STREAM_ID),
            crtc_id: nonzero32(CRTC_ID),
            mode_generation: nonzero64(MODE_GENERATION),
            width: nonzero32(1920),
            height: nonzero32(1080),
            refresh_hz: nonzero32(60),
            cursor_mode: CursorCaptureMode::IncludeInFrame,
        }
    }

    fn test_buffer(
        synchronization: CaptureSynchronization,
        state: TrackedBufferState,
    ) -> TrackedBuffer {
        let synchronization = match synchronization {
            CaptureSynchronization::Implicit => TrackedSynchronization::Implicit,
            CaptureSynchronization::Explicit => {
                TrackedSynchronization::Explicit(OwnedSyncobjPair {
                    ready_handle: nonzero32(10),
                    reuse_handle: nonzero32(11),
                })
            }
        };
        TrackedBuffer {
            id: nonzero32(BUFFER_ID),
            framebuffer_id: nonzero32(FRAMEBUFFER_ID),
            synchronization,
            next_ready_point: 1,
            last_release_point: 0,
            state,
            owned_framebuffer: None,
        }
    }

    fn test_queue(synchronization: CaptureSynchronization) -> CaptureQueue {
        CaptureQueue {
            stream_id: nonzero32(STREAM_ID),
            buffer_id: nonzero32(BUFFER_ID),
            user_data: nonzero64(USER_DATA),
            ready_point: matches!(synchronization, CaptureSynchronization::Explicit)
                .then(|| nonzero64(1)),
            reuse_point: None,
        }
    }

    fn test_grant_state_event(
        grant_id: u32,
        state: GrantState,
        status: i32,
    ) -> super::super::GrantStateEvent {
        super::super::GrantStateEvent {
            grant_id,
            state,
            status,
            timestamp_ns: 123,
        }
    }

    fn ready_for(queue: CaptureQueue) -> CaptureReady {
        CaptureReady {
            stream_id: queue.stream_id,
            buffer_id: queue.buffer_id,
            user_data: queue.user_data,
            ready_point: queue.ready_point,
        }
    }

    fn explicit_sync() -> CaptureSynchronization {
        CaptureSynchronization::Explicit
    }

    fn test_frame(
        mode_generation: u64,
        user_data: u64,
        status: i32,
        flags: u32,
    ) -> CaptureFrameEvent {
        CaptureFrameEvent {
            user_data,
            sequence: 5,
            timestamp_ns: 99,
            mode_generation,
            stream_id: STREAM_ID,
            buffer_id: BUFFER_ID,
            status,
            flags,
            dropped_frames: 0,
            damage_x: 0,
            damage_y: 0,
            damage_width: 1920,
            damage_height: 1080,
            cursor_serial: 0,
            cursor_flags: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_hotspot_x: 0,
            cursor_hotspot_y: 0,
            cursor_width: 0,
            cursor_height: 0,
        }
    }

    fn buffer_state(client: &CastKmsClient) -> CaptureBufferState {
        client.capture.active.as_ref().unwrap().buffers[0]
            .state
            .public()
    }

    fn nonzero32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nonzero64(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }
}
