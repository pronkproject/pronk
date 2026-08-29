use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use gstreamer as gst;
use thiserror::Error;

pub const MAX_MEDIA_ERROR_BYTES: usize = 512;
pub const MAX_ENCODED_ACCESS_UNIT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ENCODED_AUDIO_PACKET_BYTES: usize = 64 * 1024;
pub const MAX_ENCODED_OUTPUT_CAPACITY: usize = 64;
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_CHANNELS: u32 = 2;
pub const OPUS_BITRATE: u32 = 128_000;
pub const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub struct PipeWireVideoInput {
    pub remote: OwnedFd,
    pub node_name: String,
    pub object_serial: NonZeroU64,
    pub caps: String,
}

#[derive(Debug)]
pub struct PipeWireAudioInput {
    pub remote: OwnedFd,
    pub node_name: String,
    pub object_serial: NonZeroU64,
    pub caps: String,
}

#[derive(Debug)]
pub struct MediaGraphConfiguration {
    pub media_generation: NonZeroU64,
    pub video: PipeWireVideoInput,
    pub audio: Option<PipeWireAudioInput>,
    pub video_bitrate: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedVideoCaps {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub framerate_numerator: NonZeroU32,
    pub framerate_denominator: NonZeroU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedAudioCaps {
    pub sample_rate: NonZeroU32,
    pub channels: NonZeroU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFrameDependency {
    KeyFrame,
    Delta,
}

/// One complete H.264 Annex-B access unit from the backend encoder.
#[derive(Debug, Clone)]
pub struct EncodedVideoAccessUnit {
    pub media_generation: NonZeroU64,
    pub dependency: VideoFrameDependency,
    pub data: Vec<u8>,
    pub media_timestamp: Duration,
    pub reference_time: Instant,
    pub duration: Duration,
}

/// One raw Opus packet with generation-relative timing.
#[derive(Debug, Clone)]
pub struct EncodedAudioPacket {
    pub media_generation: NonZeroU64,
    pub data: Vec<u8>,
    pub media_timestamp: Duration,
    pub reference_time: Instant,
    pub duration: Duration,
}

impl ValidatedVideoCaps {
    pub fn parse(value: &str) -> Result<Self, MediaGraphError> {
        gst::init().map_err(|error| {
            MediaGraphError::new(format!(
                "initialize GStreamer while validating video caps: {error}"
            ))
        })?;
        let caps = value
            .parse::<gst::Caps>()
            .map_err(|error| MediaGraphError::new(format!("parse video caps: {error}")))?;
        validate_caps(&caps)
    }
}

impl ValidatedAudioCaps {
    pub fn parse(value: &str) -> Result<Self, MediaGraphError> {
        gst::init().map_err(|error| {
            MediaGraphError::new(format!(
                "initialize GStreamer while validating audio caps: {error}"
            ))
        })?;
        let caps = value
            .parse::<gst::Caps>()
            .map_err(|error| MediaGraphError::new(format!("parse audio caps: {error}")))?;
        validate_audio_caps(&caps)
    }
}

pub(crate) fn parse_audio_caps(
    value: &str,
) -> Result<(gst::Caps, ValidatedAudioCaps), MediaGraphError> {
    let caps = value
        .parse::<gst::Caps>()
        .map_err(|error| MediaGraphError::new(format!("parse audio caps: {error}")))?;
    let validated = validate_audio_caps(&caps)?;
    Ok((caps, validated))
}

pub(crate) fn parse_caps(value: &str) -> Result<(gst::Caps, ValidatedVideoCaps), MediaGraphError> {
    let caps = value
        .parse::<gst::Caps>()
        .map_err(|error| MediaGraphError::new(format!("parse video caps: {error}")))?;
    let validated = validate_caps(&caps)?;
    Ok((caps, validated))
}

fn validate_caps(caps: &gst::Caps) -> Result<ValidatedVideoCaps, MediaGraphError> {
    if caps.size() != 1 || !caps.is_fixed() {
        return Err(MediaGraphError::new(
            "video caps must contain exactly one fixed structure",
        ));
    }
    let structure = caps
        .structure(0)
        .ok_or_else(|| MediaGraphError::new("video caps have no structure"))?;
    if structure.name().as_str() != "video/x-raw" {
        return Err(MediaGraphError::new("video caps are not raw video"));
    }
    let format = structure
        .get::<&str>("format")
        .map_err(|_| MediaGraphError::new("video caps have no fixed string format"))?;
    if format != "BGRx" {
        return Err(MediaGraphError::new(format!(
            "video caps format {format:?} is unsupported; expected BGRx"
        )));
    }
    let width = positive_caps_integer(structure, "width")?;
    let height = positive_caps_integer(structure, "height")?;
    let framerate = structure
        .get::<gst::Fraction>("framerate")
        .map_err(|_| MediaGraphError::new("video caps have no fixed framerate"))?;
    let numerator = u32::try_from(framerate.numer())
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| MediaGraphError::new("video caps have an invalid framerate numerator"))?;
    let denominator = u32::try_from(framerate.denom())
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| MediaGraphError::new("video caps have an invalid framerate denominator"))?;
    Ok(ValidatedVideoCaps {
        width,
        height,
        framerate_numerator: numerator,
        framerate_denominator: denominator,
    })
}

fn validate_audio_caps(caps: &gst::Caps) -> Result<ValidatedAudioCaps, MediaGraphError> {
    if caps.size() != 1 || !caps.is_fixed() {
        return Err(MediaGraphError::new(
            "audio caps must contain exactly one fixed structure",
        ));
    }
    let structure = caps
        .structure(0)
        .ok_or_else(|| MediaGraphError::new("audio caps have no structure"))?;
    if structure.name().as_str() != "audio/x-raw" {
        return Err(MediaGraphError::new("audio caps are not raw audio"));
    }
    let format = structure
        .get::<&str>("format")
        .map_err(|_| MediaGraphError::new("audio caps have no fixed string format"))?;
    if format != "S16LE" {
        return Err(MediaGraphError::new(format!(
            "audio caps format {format:?} is unsupported; expected S16LE"
        )));
    }
    let layout = structure
        .get::<&str>("layout")
        .map_err(|_| MediaGraphError::new("audio caps have no fixed layout"))?;
    if layout != "interleaved" {
        return Err(MediaGraphError::new(format!(
            "audio caps layout {layout:?} is unsupported; expected interleaved"
        )));
    }
    let sample_rate = positive_caps_integer(structure, "rate")?;
    let channels = positive_caps_integer(structure, "channels")?;
    if sample_rate.get() != OPUS_SAMPLE_RATE || channels.get() != OPUS_CHANNELS {
        return Err(MediaGraphError::new(format!(
            "audio caps are {} Hz with {} channels; expected {OPUS_SAMPLE_RATE} Hz stereo",
            sample_rate, channels
        )));
    }
    Ok(ValidatedAudioCaps {
        sample_rate,
        channels,
    })
}

fn positive_caps_integer(
    structure: &gst::StructureRef,
    field: &'static str,
) -> Result<NonZeroU32, MediaGraphError> {
    structure
        .get::<i32>(field)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .and_then(NonZeroU32::new)
        .ok_or_else(|| MediaGraphError::new(format!("video caps have an invalid {field}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaGraphState {
    Empty,
    Configured,
    Streaming,
    Suspended,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaGraphStatistics {
    pub frames: u64,
    pub dropped_frames: u64,
    pub key_frames: u64,
    pub encoded_bytes: u64,
    pub video_bitrate: u64,
    pub key_frame_requests: u64,
    pub bitrate_changes: u64,
    pub bytes_hashed: u64,
    pub last_frame_hash: Option<[u8; 32]>,
    pub first_pts_nanos: Option<u64>,
    pub last_pts_nanos: Option<u64>,
    pub encoder_name: Option<String>,
    pub encoded_caps: Option<String>,
    pub audio_packets: u64,
    pub dropped_audio_packets: u64,
    pub encoded_audio_bytes: u64,
    pub first_audio_pts_nanos: Option<u64>,
    pub last_audio_pts_nanos: Option<u64>,
    pub audio_encoder_name: Option<String>,
    pub encoded_audio_caps: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaGraphSnapshot {
    pub revision: u64,
    pub media_generation: Option<NonZeroU64>,
    pub state: MediaGraphState,
    pub statistics: MediaGraphStatistics,
    pub last_error: Option<String>,
}

impl MediaGraphSnapshot {
    pub(crate) fn empty() -> Self {
        Self {
            revision: 1,
            media_generation: None,
            state: MediaGraphState::Empty,
            statistics: MediaGraphStatistics::default(),
            last_error: None,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct MediaGraphError(String);

impl MediaGraphError {
    pub fn new(message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_MEDIA_ERROR_BYTES {
            let mut boundary = MAX_MEDIA_ERROR_BYTES;
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message.truncate(boundary);
        }
        Self(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_one_fixed_bgrx_video_format() {
        let parsed = ValidatedVideoCaps::parse(
            "video/x-raw,format=BGRx,width=1920,height=1080,framerate=60/1",
        )
        .unwrap();
        assert_eq!(parsed.width.get(), 1920);
        assert_eq!(parsed.height.get(), 1080);
        assert_eq!(parsed.framerate_numerator.get(), 60);
        assert_eq!(parsed.framerate_denominator.get(), 1);

        for invalid in [
            "video/x-raw,format=NV12,width=1920,height=1080,framerate=60/1",
            "video/x-raw,format=BGRx,width=[1,1920],height=1080,framerate=60/1",
            "audio/x-raw,format=S16LE,rate=48000,channels=2",
        ] {
            assert!(ValidatedVideoCaps::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn accepts_only_fixed_opus_input_audio_format() {
        let parsed = ValidatedAudioCaps::parse(
            "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2",
        )
        .unwrap();
        assert_eq!(parsed.sample_rate.get(), OPUS_SAMPLE_RATE);
        assert_eq!(parsed.channels.get(), OPUS_CHANNELS);

        for invalid in [
            "audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2",
            "audio/x-raw,format=S16LE,layout=interleaved,rate=44100,channels=2",
            "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=[1,2]",
            "video/x-raw,format=BGRx,width=1920,height=1080,framerate=60/1",
        ] {
            assert!(ValidatedAudioCaps::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn errors_are_utf8_bounded() {
        let error = MediaGraphError::new("é".repeat(MAX_MEDIA_ERROR_BYTES));
        assert!(error.to_string().len() <= MAX_MEDIA_ERROR_BYTES);
        assert!(error.to_string().is_char_boundary(error.to_string().len()));
    }
}
