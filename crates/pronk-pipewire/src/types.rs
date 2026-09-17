use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::OwnedFd;
use std::time::Duration;

use thiserror::Error;

pub const MIN_VIDEO_BUFFERS: usize = 2;
pub const MAX_VIDEO_BUFFERS: usize = 64;
pub const MAX_FRAME_DIMENSION: u32 = 8192;
pub const MAX_IDENTITY_STRING_BYTES: usize = 256;

/// Byte layout of the single packed pixel plane, independent of its modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPixelFormat {
    Xrgb8888,
    /// The producer supplies meaningful alpha, including opaque alpha for video.
    Argb8888,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoBufferStorage {
    /// Linear packed storage suitable for CPU mapping.
    MappableLinear,
    /// Single-plane packed storage described by the allocating graphics API.
    ///
    /// Even modifier zero is explicit here. The transport does not promise CPU
    /// mapping or derive a tiled allocation's extent from pitch and height.
    /// The caller must obtain a valid single-memory-plane layout from its
    /// graphics API; modifiers requiring auxiliary planes are not supported.
    DrmModifier { modifier: u64, offset: u32 },
}

impl VideoBufferStorage {
    pub(crate) fn offset(self) -> u32 {
        match self {
            Self::MappableLinear => 0,
            Self::DrmModifier { offset, .. } => offset,
        }
    }
}

/// A caller-validated packed allocation; `size` includes any plane offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoBufferLayout {
    pub format: VideoPixelFormat,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub pitch: NonZeroU32,
    pub size: NonZeroU64,
    pub storage: VideoBufferStorage,
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

/// A positive rational video frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameRate {
    numerator: NonZeroU32,
    denominator: NonZeroU32,
}

impl VideoFrameRate {
    pub fn new(numerator: NonZeroU32, denominator: NonZeroU32) -> Self {
        let divisor = greatest_common_divisor(numerator.get(), denominator.get());
        Self {
            numerator: NonZeroU32::new(numerator.get() / divisor)
                .expect("a positive numerator remains positive after reduction"),
            denominator: NonZeroU32::new(denominator.get() / divisor)
                .expect("a positive denominator remains positive after reduction"),
        }
    }

    pub const fn integer(frames_per_second: NonZeroU32) -> Self {
        Self {
            numerator: frames_per_second,
            denominator: NonZeroU32::MIN,
        }
    }

    pub const fn numerator(self) -> NonZeroU32 {
        self.numerator
    }

    pub const fn denominator(self) -> NonZeroU32 {
        self.denominator
    }

    pub fn frame_interval(self) -> Duration {
        let nanoseconds = u64::from(self.denominator.get())
            .saturating_mul(1_000_000_000)
            .div_ceil(u64::from(self.numerator.get()));
        Duration::from_nanos(nanoseconds)
    }

    pub(crate) fn matches_fraction(self, numerator: u32, denominator: u32) -> bool {
        numerator != 0
            && denominator != 0
            && u64::from(self.numerator.get()) * u64::from(denominator)
                == u64::from(numerator) * u64::from(self.denominator.get())
    }
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSourceConfig {
    pub node_name: String,
    pub node_description: String,
    pub session_id: String,
    pub device_instance: String,
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub media_generation: NonZeroU64,
    pub frame_rate: VideoFrameRate,
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
        expected.validate()?;
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

impl VideoBufferLayout {
    /// Validate the complete layout shared by every PipeWire buffer.
    pub fn validate(self) -> Result<(), ConfigurationError> {
        let width = self.width.get();
        let height = self.height.get();
        if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
            return Err(ConfigurationError::FrameDimensions { width, height });
        }
        if self.pitch.get() > i32::MAX as u32 {
            return Err(ConfigurationError::InvalidPitch(self.pitch.get()));
        }
        if self.size.get() > i32::MAX as u64 {
            return Err(ConfigurationError::InvalidSize(self.size.get()));
        }
        let offset = u64::from(self.storage.offset());
        if offset >= self.size.get() {
            return Err(ConfigurationError::InvalidOffset(self.storage.offset()));
        }
        let linear = match self.storage {
            VideoBufferStorage::MappableLinear => true,
            VideoBufferStorage::DrmModifier { modifier, .. } => {
                // DRM_FORMAT_MOD_INVALID is a negotiation sentinel, not an image layout.
                if modifier == 0x00ff_ffff_ffff_ffff {
                    return Err(ConfigurationError::InvalidModifier(modifier));
                }
                modifier == 0
            }
        };
        if linear {
            if self.pitch.get() < width * 4 {
                return Err(ConfigurationError::InvalidPitch(self.pitch.get()));
            }
            let minimum_size = offset + u64::from(self.pitch.get()) * u64::from(height);
            if self.size.get() < minimum_size {
                return Err(ConfigurationError::InvalidSize(self.size.get()));
            }
        }
        Ok(())
    }
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
    #[error("DRM modifier {0:#x} is not a concrete image layout")]
    InvalidModifier(u64),
    #[error("video plane offset {0} is outside its allocation")]
    InvalidOffset(u32),
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
    ReadyBeforePublish,
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
        /// PipeWire no longer retains this use. In ready-before-publish
        /// transport, the caller must still establish native reader completion
        /// before overwriting GPU storage; this event is not itself a GPU fence.
        buffer_id: NonZeroU32,
        /// Sequence retained from the submitted frame, not consumer metadata.
        sequence: u64,
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
    #[error("ready-before-publish buffer {0} carries an acquire point")]
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

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn fractional_frame_rate_preserves_its_period() {
        let rate = VideoFrameRate::new(
            NonZeroU32::new(30_000).unwrap(),
            NonZeroU32::new(1_001).unwrap(),
        );

        assert_eq!(rate.frame_interval(), Duration::from_nanos(33_366_667));
    }

    #[test]
    fn frame_rate_reduces_equivalent_fractions() {
        let rate = VideoFrameRate::new(
            NonZeroU32::new(60_000).unwrap(),
            NonZeroU32::new(2_002).unwrap(),
        );

        assert_eq!(rate.numerator().get(), 30_000);
        assert_eq!(rate.denominator().get(), 1_001);
        assert!(rate.matches_fraction(90_000, 3_003));
        assert!(!rate.matches_fraction(30_000, 0));
        assert!(!rate.matches_fraction(30_001, 1_001));
    }

    fn layout(storage: VideoBufferStorage) -> VideoBufferLayout {
        VideoBufferLayout {
            format: crate::VideoPixelFormat::Xrgb8888,
            width: NonZeroU32::new(16).unwrap(),
            height: NonZeroU32::new(8).unwrap(),
            pitch: NonZeroU32::new(64).unwrap(),
            size: NonZeroU64::new(512).unwrap(),
            storage,
        }
    }

    #[test]
    fn linear_profiles_require_complete_rows_after_the_offset() {
        for storage in [
            VideoBufferStorage::MappableLinear,
            VideoBufferStorage::DrmModifier {
                modifier: 0,
                offset: 0,
            },
        ] {
            let mut image = layout(storage);
            assert!(image.validate().is_ok());
            image.size = NonZeroU64::new(511).unwrap();
            assert!(matches!(
                image.validate(),
                Err(ConfigurationError::InvalidSize(511))
            ));
            image.size = NonZeroU64::new(512).unwrap();
            image.pitch = NonZeroU32::new(63).unwrap();
            assert!(matches!(
                image.validate(),
                Err(ConfigurationError::InvalidPitch(63))
            ));
        }
        let mut image = layout(VideoBufferStorage::DrmModifier {
            modifier: 0,
            offset: 64,
        });
        assert!(matches!(
            image.validate(),
            Err(ConfigurationError::InvalidSize(512))
        ));
        image.size = NonZeroU64::new(576).unwrap();
        assert!(image.validate().is_ok());
    }

    #[test]
    fn opaque_layouts_validate_transport_bounds_without_inventing_linear_extents() {
        let mut image = layout(VideoBufferStorage::DrmModifier {
            modifier: 0x0100_0000_0000_0009,
            offset: 64,
        });
        // Only the graphics API interprets the modifier's byte geometry.
        image.pitch = NonZeroU32::new(16).unwrap();
        image.size = NonZeroU64::new(128).unwrap();
        assert!(image.validate().is_ok());
        image.size = NonZeroU64::new(64).unwrap();
        assert!(matches!(
            image.validate(),
            Err(ConfigurationError::InvalidOffset(64))
        ));
        image.size = NonZeroU64::new(i32::MAX as u64 + 1).unwrap();
        assert!(matches!(
            image.validate(),
            Err(ConfigurationError::InvalidSize(_))
        ));
        image.size = NonZeroU64::new(512).unwrap();
        image.pitch = NonZeroU32::new(i32::MAX as u32 + 1).unwrap();
        assert!(matches!(
            image.validate(),
            Err(ConfigurationError::InvalidPitch(_))
        ));
    }

    #[test]
    fn invalid_modifier_and_oversized_dimensions_are_rejected() {
        let mut image = layout(VideoBufferStorage::DrmModifier {
            modifier: 0x00ff_ffff_ffff_ffff,
            offset: 0,
        });
        assert!(matches!(
            image.validate(),
            Err(ConfigurationError::InvalidModifier(_))
        ));
        image.storage = VideoBufferStorage::MappableLinear;
        image.width = NonZeroU32::new(MAX_FRAME_DIMENSION + 1).unwrap();
        assert!(matches!(
            image.validate(),
            Err(ConfigurationError::FrameDimensions { .. })
        ));
    }
}
