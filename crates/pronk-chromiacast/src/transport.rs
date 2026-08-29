use std::fmt::Debug;
use std::num::NonZeroU32;
use std::time::Duration;

use async_trait::async_trait;
use pronk_media::{EncodedAudioPacket, EncodedVideoAccessUnit};
use thiserror::Error;
use tokio::sync::watch;

const MAX_TRANSPORT_ERROR_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoTransportConfiguration {
    pub width: u32,
    pub height: u32,
    pub framerate_numerator: u32,
    pub framerate_denominator: u32,
    pub bitrate: u32,
    pub audio: Option<AudioTransportConfiguration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioTransportConfiguration {
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoSendOutcome {
    Accepted,
    Congested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioSendOutcome {
    Accepted,
    Congested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct VideoTransportPressure {
    pub in_flight_frames: usize,
    pub in_flight_media_duration: Duration,
    pub max_acceptable_in_flight_duration: Duration,
    pub current_rtt: Option<Duration>,
    pub receiver_playout_delay: Option<Duration>,
    pub nack_count: u64,
    pub frames_dropped_or_skipped: u64,
    pub fraction_lost: Option<u8>,
}

impl VideoTransportPressure {
    pub(crate) fn queue_saturated(self) -> bool {
        !self.max_acceptable_in_flight_duration.is_zero()
            && self.in_flight_media_duration >= self.max_acceptable_in_flight_duration
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct VideoTransportFeedbackSnapshot {
    pub revision: u64,
    pub key_frame_requests: u64,
    pub acknowledged_frames: u64,
    pub acknowledged_audio_packets: u64,
    pub pressure: Option<VideoTransportPressure>,
    pub terminal_error: Option<VideoTransportError>,
}

pub(crate) struct NegotiatedVideoTransport {
    pub sender: Box<dyn VideoSenderPort>,
    pub audio_sender: Option<Box<dyn AudioSenderPort>>,
    pub feedback: watch::Receiver<VideoTransportFeedbackSnapshot>,
    pub minimum_bitrate: Option<NonZeroU32>,
}

impl Debug for NegotiatedVideoTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NegotiatedVideoTransport")
            .field("feedback", &self.feedback.borrow())
            .field("has_audio_sender", &self.audio_sender.is_some())
            .field("minimum_bitrate", &self.minimum_bitrate)
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub(crate) trait AudioSenderPort: Debug + Send + 'static {
    async fn send(
        &mut self,
        packet: EncodedAudioPacket,
    ) -> Result<AudioSendOutcome, VideoTransportError>;
    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError>;
}

#[async_trait]
pub(crate) trait VideoSenderPort: Debug + Send + 'static {
    fn supports_target_playout_delay_updates(&self) -> bool {
        false
    }
    fn maximum_target_playout_delay(&self) -> Option<Duration> {
        None
    }
    async fn set_target_playout_delay(
        &mut self,
        _delay: Duration,
    ) -> Result<(), VideoTransportError> {
        Err(VideoTransportError::new(
            "video transport does not support target playout-delay updates",
        ))
    }
    async fn send(
        &mut self,
        access_unit: EncodedVideoAccessUnit,
    ) -> Result<VideoSendOutcome, VideoTransportError>;
    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError>;
}

#[async_trait]
pub(crate) trait VideoTransportNegotiator: Debug + Send + Sync + 'static {
    async fn negotiate_video(
        &mut self,
        configuration: VideoTransportConfiguration,
    ) -> Result<NegotiatedVideoTransport, VideoTransportError>;
    async fn stop_video(&mut self) -> Result<(), VideoTransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub(crate) struct VideoTransportError(String);

impl VideoTransportError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_TRANSPORT_ERROR_BYTES {
            let mut boundary = MAX_TRANSPORT_ERROR_BYTES;
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message.truncate(boundary);
        }
        Self(message)
    }
}
