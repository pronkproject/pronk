use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::OwnedFd;

use thiserror::Error;

pub const MIN_VIDEO_BUFFERS: usize = 2;
pub const MAX_VIDEO_BUFFERS: usize = 64;
pub const MAX_FRAME_DIMENSION: u32 = 8192;
pub const MAX_IDENTITY_STRING_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoBufferLayout {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub pitch: NonZeroU32,
    pub size: NonZeroU64,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct VideoSyncTimelines {
    pub ready: OwnedFd,
    pub reuse: OwnedFd,
}

/// One caller-owned capture buffer exported to the PipeWire producer.
///
/// The descriptor set contains no DRM primary-node or grant descriptor. The
/// optional opaque syncobj descriptors are meaningful only with the exact
/// timeline points supplied in [`VideoFrame`].
#[derive(Debug)]
pub struct VideoBuffer {
    pub id: NonZeroU32,
    pub dma_buf: OwnedFd,
    pub layout: VideoBufferLayout,
    pub timelines: Option<VideoSyncTimelines>,
}

#[derive(Debug)]
pub enum PipeWireRemote {
    /// A preconnected server-classified PipeWire native-protocol socket.
    Connected(OwnedFd),
    /// Explicit opt-in for isolated development and VM correctness gates.
    AmbientDevelopment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSourceConfig {
    pub node_name: String,
    pub node_description: String,
    pub session_id: String,
    pub device_instance: String,
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub grant_id: NonZeroU32,
    pub media_generation: NonZeroU64,
    pub refresh_hz: NonZeroU32,
}

impl VideoSourceConfig {
    pub(crate) fn validate(&self, buffers: &[VideoBuffer]) -> Result<(), ConfigurationError> {
        validate_string("node name", &self.node_name)?;
        validate_string("node description", &self.node_description)?;
        validate_string("session ID", &self.session_id)?;
        validate_string("device instance", &self.device_instance)?;
        if buffers.len() < MIN_VIDEO_BUFFERS || buffers.len() > MAX_VIDEO_BUFFERS {
            return Err(ConfigurationError::BufferCount(buffers.len()));
        }

        let expected = buffers[0].layout;
        validate_layout(expected)?;
        let explicit = buffers[0].timelines.is_some();
        for (index, buffer) in buffers.iter().enumerate() {
            if buffer.layout != expected {
                return Err(ConfigurationError::LayoutMismatch(index));
            }
            if buffer.timelines.is_some() != explicit {
                return Err(ConfigurationError::SynchronizationMismatch(index));
            }
            if buffers[..index]
                .iter()
                .any(|previous| previous.id == buffer.id)
            {
                return Err(ConfigurationError::DuplicateBufferId(buffer.id.get()));
            }
        }
        Ok(())
    }
}

fn validate_string(field: &'static str, value: &str) -> Result<(), ConfigurationError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_STRING_BYTES || value.contains('\0') {
        return Err(ConfigurationError::InvalidString { field });
    }
    Ok(())
}

fn validate_layout(layout: VideoBufferLayout) -> Result<(), ConfigurationError> {
    let width = layout.width.get();
    let height = layout.height.get();
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(ConfigurationError::FrameDimensions { width, height });
    }
    if layout.modifier != 0 {
        return Err(ConfigurationError::NonlinearModifier(layout.modifier));
    }
    if layout.pitch.get() < width.saturating_mul(4) || layout.pitch.get() > i32::MAX as u32 {
        return Err(ConfigurationError::InvalidPitch(layout.pitch.get()));
    }
    let minimum_size = u64::from(layout.pitch.get()) * u64::from(height);
    if layout.size.get() < minimum_size || layout.size.get() > i32::MAX as u64 {
        return Err(ConfigurationError::InvalidSize(layout.size.get()));
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConfigurationError {
    #[error("{field} is empty, too long, or contains NUL")]
    InvalidString { field: &'static str },
    #[error("PipeWire video source requires 2..=64 buffers; got {0}")]
    BufferCount(usize),
    #[error("video buffer {0} has a different layout")]
    LayoutMismatch(usize),
    #[error("video buffer {0} has a different synchronization mode")]
    SynchronizationMismatch(usize),
    #[error("duplicate video buffer ID {0}")]
    DuplicateBufferId(u32),
    #[error("frame dimensions {width}x{height} exceed the supported bound")]
    FrameDimensions { width: u32, height: u32 },
    #[error("only the linear modifier is currently supported; got {0:#x}")]
    NonlinearModifier(u64),
    #[error("video buffer pitch {0} is invalid")]
    InvalidPitch(u32),
    #[error("video buffer size {0} is invalid")]
    InvalidSize(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoDamage {
    pub x: u32,
    pub y: u32,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl VideoDamage {
    pub(crate) fn is_bounded_by(self, layout: VideoBufferLayout) -> bool {
        self.x <= layout.width.get()
            && self.width.get() <= layout.width.get().saturating_sub(self.x)
            && self.y <= layout.height.get()
            && self.height.get() <= layout.height.get().saturating_sub(self.y)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrame {
    pub buffer_id: NonZeroU32,
    pub sequence: u64,
    pub pts_ns: i64,
    pub damage: VideoDamage,
    pub discontinuity: bool,
    /// Exact CastKMS ready point for a sync-timeline handoff. The point may
    /// already be signaled when a producer deliberately waits before
    /// publication, but it must still identify this exact buffer use.
    pub acquire_point: Option<NonZeroU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeWireBufferTransport {
    /// The producer must submit only after readiness is established itself.
    Waited,
    /// PipeWire receives ready/reuse syncobj fds and the exact timeline point.
    SyncTimeline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoNodeIdentity {
    pub node_name: String,
    pub object_id: NonZeroU32,
    pub object_serial: NonZeroU64,
    pub media_generation: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSourceEvent {
    BufferAvailable {
        buffer_id: NonZeroU32,
        transport: PipeWireBufferTransport,
    },
    BufferReleased {
        buffer_id: NonZeroU32,
    },
    Failed(VideoSourceRuntimeError),
    Stopped,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VideoSourceRuntimeError {
    #[error("create or connect PipeWire object: {0}")]
    PipeWire(String),
    #[error("PipeWire core error {code}: {message}")]
    Core { code: i32, message: String },
    #[error("PipeWire stream error: {0}")]
    Stream(String),
    #[error("PipeWire source node disappeared")]
    NodeRemoved,
    #[error("the versioned WirePlumber private-media policy is unavailable")]
    PolicyUnavailable,
    #[error("PipeWire negotiated an unsupported video format")]
    UnsupportedFormat,
    #[error("PipeWire supplied an invalid buffer: {0}")]
    InvalidPipeWireBuffer(&'static str),
    #[error("PipeWire supplied more buffers than the caller-owned pool")]
    TooManyPipeWireBuffers,
    #[error("PipeWire buffer ownership is invalid for buffer {0}")]
    InvalidOwnership(u32),
    #[error("frame references unknown buffer {0}")]
    UnknownBuffer(u32),
    #[error("frame damage is outside buffer {0}")]
    InvalidDamage(u32),
    #[error("buffer {0} requires a sync-timeline acquire point")]
    MissingAcquirePoint(u32),
    #[error("buffer {0} was waited but carries an acquire point")]
    UnexpectedAcquirePoint(u32),
    #[error("buffer {buffer_id} returned release point {actual}; expected {expected}")]
    ReleasePointMismatch {
        buffer_id: u32,
        expected: u64,
        actual: u64,
    },
    #[error("PipeWire source event queue overflowed")]
    EventQueueFull,
    #[error("PipeWire source loop panicked")]
    ThreadPanicked,
}
