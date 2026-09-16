use std::num::NonZeroU64;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::h264;
use crate::model::{MediaGraphError, VideoCadence, VideoCodec, VideoEncoder, VideoFrameDependency};
use crate::vp8;

impl VideoCodec {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::H264 => "H.264",
        }
    }

    pub(crate) fn output_caps(self) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Vp8 => vp8::encoder_output_caps(),
            Self::H264 => h264::encoder_output_caps(),
        }
    }

    pub(crate) fn build_parser(self) -> Result<Option<gst::Element>, MediaGraphError> {
        match self {
            Self::Vp8 => Ok(None),
            Self::H264 => gst::ElementFactory::make("h264parse")
                .name("pronk-h264-parser")
                .property("config-interval", -1_i32)
                .property("disable-passthrough", true)
                .build()
                .map(Some)
                .map_err(|error| MediaGraphError::new(format!("construct H.264 parser: {error}"))),
        }
    }

    pub(crate) fn validate_caps(self, caps: &gst::CapsRef) -> Result<(), MediaGraphError> {
        match self {
            Self::Vp8 => vp8::validate_caps(caps),
            Self::H264 => h264::validate_caps(caps),
        }
    }

    pub(crate) fn validate_frame(
        self,
        bytes: &[u8],
        dependency: VideoFrameDependency,
        first: bool,
    ) -> Result<(), MediaGraphError> {
        match self {
            Self::Vp8 => vp8::validate_frame(bytes, dependency, first),
            Self::H264 => h264::validate_access_unit(bytes, dependency, first),
        }
    }
}

impl VideoEncoder {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Software(VideoCodec::Vp8) => vp8::ENCODER_NAME,
            Self::Software(VideoCodec::H264) => h264::ENCODER_NAME,
        }
    }

    pub(crate) fn input_caps(self, cadence: VideoCadence) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => vp8::encoder_input_caps(cadence),
            Self::Software(VideoCodec::H264) => h264::encoder_input_caps(cadence),
        }
    }

    pub(crate) fn build(
        self,
        bitrate: NonZeroU64,
        cadence: VideoCadence,
    ) -> Result<gst::Element, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => gst::ElementFactory::make(vp8::ENCODER_NAME)
                .name("pronk-vp8-encoder")
                .property("deadline", 1_i64)
                .property("cpu-used", 8_i32)
                .property_from_str("end-usage", "cbr")
                .property("undershoot", 100_i32)
                .property("overshoot", 15_i32)
                .property("buffer-initial-size", 500_i32)
                .property("buffer-optimal-size", 600_i32)
                .property("buffer-size", 1_000_i32)
                .property("target-bitrate", vp8::bitrate(bitrate.get())?)
                .property_from_str("keyframe-mode", "disabled")
                .property("keyframe-max-dist", vp8::key_frame_interval(cadence))
                .property("lag-in-frames", 0_i32)
                .property("threads", 8_i32)
                .property("static-threshold", 100_i32)
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct {}: {error}", vp8::ENCODER_NAME))
                }),
            Self::Software(VideoCodec::H264) => gst::ElementFactory::make(h264::ENCODER_NAME)
                .name("pronk-h264-encoder")
                .property_from_str("tune", "zerolatency")
                .property_from_str("speed-preset", "ultrafast")
                .property("bitrate", h264::bitrate_kbits(bitrate.get())?)
                .property("key-int-max", h264::key_frame_interval(cadence))
                .property("bframes", 0_u32)
                .property("byte-stream", true)
                .property("aud", true)
                .property("sliced-threads", true)
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct {}: {error}", h264::ENCODER_NAME))
                }),
        }
    }

    pub(crate) fn effective_bitrate(self, bitrate: NonZeroU64) -> Result<u64, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => u64::try_from(vp8::bitrate(bitrate.get())?)
                .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative")),
            Self::Software(VideoCodec::H264) => {
                Ok(u64::from(h264::bitrate_kbits(bitrate.get())?).saturating_mul(1_000))
            }
        }
    }

    pub(crate) fn set_bitrate(
        self,
        encoder: &gst::Element,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => {
                let bitrate = vp8::bitrate(bitrate.get())?;
                encoder.set_property("target-bitrate", bitrate);
                u64::try_from(bitrate)
                    .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative"))
            }
            Self::Software(VideoCodec::H264) => {
                let bitrate = h264::bitrate_kbits(bitrate.get())?;
                encoder.set_property("bitrate", bitrate);
                Ok(u64::from(bitrate).saturating_mul(1_000))
            }
        }
    }
}
