use std::num::NonZeroU64;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::h264;
use crate::model::{
    MediaGraphError, VideoCadence, VideoCodec, VideoEncoder, VideoFrameDependency, VideoInputLayout,
};
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
    pub(crate) fn validate_input(&self, layout: &VideoInputLayout) -> Result<(), MediaGraphError> {
        match (self, layout) {
            (Self::Software(_), VideoInputLayout::SystemMemoryBgrx) => Ok(()),
            (Self::Software(_), VideoInputLayout::DmaBuf { .. }) => Err(MediaGraphError::new(
                "the software video encoder requires system-memory BGRx input",
            )),
            (Self::VaH264 { .. }, VideoInputLayout::DmaBuf { .. }) => Ok(()),
            (Self::VaH264 { .. }, VideoInputLayout::SystemMemoryBgrx) => Err(MediaGraphError::new(
                "the VA H.264 encoder requires DMA-BUF input",
            )),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Software(VideoCodec::Vp8) => vp8::ENCODER_NAME,
            Self::Software(VideoCodec::H264) => h264::ENCODER_NAME,
            Self::VaH264 { .. } => "vah264enc",
        }
    }

    pub(crate) fn input_caps(&self, cadence: VideoCadence) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => vp8::encoder_input_caps(cadence),
            Self::Software(VideoCodec::H264) => h264::encoder_input_caps(cadence),
            Self::VaH264 { .. } => format!(
                "video/x-raw(memory:VAMemory),format=(string)NV12,framerate=(fraction){}",
                cadence.caps_fraction()
            )
            .parse::<gst::Caps>()
            .map_err(|error| {
                MediaGraphError::new(format!("construct VA encoder input caps: {error}"))
            }),
        }
    }

    pub(crate) fn build_converter(&self) -> Result<gst::Element, MediaGraphError> {
        match self {
            Self::Software(_) => gst::ElementFactory::make("videoconvert")
                .name("pronk-video-convert")
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct video converter: {error}"))
                }),
            Self::VaH264 { render_node } => {
                let converter = gst::ElementFactory::make("vapostproc")
                    .name("pronk-va-video-convert")
                    .property("disable-passthrough", true)
                    .build()
                    .map_err(|error| {
                        MediaGraphError::new(format!("construct VA video converter: {error}"))
                    })?;
                validate_va_device(&converter, render_node)?;
                Ok(converter)
            }
        }
    }

    pub(crate) fn memory_path(&self) -> &'static str {
        match self {
            Self::Software(_) => "system-memory BGRx to system-memory I420",
            Self::VaH264 { .. } => "DMA-BUF DMA_DRM to VA-memory NV12",
        }
    }

    pub(crate) fn build(
        &self,
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
            Self::VaH264 { render_node } => {
                let key_frame_interval = h264::key_frame_interval(cadence);
                if key_frame_interval > 1_024 {
                    return Err(MediaGraphError::new(
                        "VA H.264 key-frame interval exceeds the encoder limit",
                    ));
                }
                let encoder = gst::ElementFactory::make("vah264enc")
                    .name("pronk-va-h264-encoder")
                    .property("bitrate", h264::bitrate_kbits(bitrate.get())?)
                    .property("key-int-max", key_frame_interval)
                    .property("b-frames", 0_u32)
                    .property("cabac", false)
                    .property("dct8x8", false)
                    .property("aud", true)
                    .property_from_str("rate-control", "cbr")
                    .build()
                    .map_err(|error| {
                        MediaGraphError::new(format!("construct vah264enc: {error}"))
                    })?;
                validate_va_device(&encoder, render_node)?;
                Ok(encoder)
            }
        }
    }

    pub(crate) fn effective_bitrate(&self, bitrate: NonZeroU64) -> Result<u64, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => u64::try_from(vp8::bitrate(bitrate.get())?)
                .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative")),
            Self::Software(VideoCodec::H264) => {
                Ok(u64::from(h264::bitrate_kbits(bitrate.get())?).saturating_mul(1_000))
            }
            Self::VaH264 { .. } => {
                Ok(u64::from(h264::bitrate_kbits(bitrate.get())?).saturating_mul(1_000))
            }
        }
    }

    pub(crate) fn set_bitrate(
        &self,
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
            Self::VaH264 { .. } => {
                let bitrate = h264::bitrate_kbits(bitrate.get())?;
                encoder.set_property("bitrate", bitrate);
                Ok(u64::from(bitrate).saturating_mul(1_000))
            }
        }
    }
}

fn validate_va_device(element: &gst::Element, requested: &Path) -> Result<(), MediaGraphError> {
    let actual = element.property::<String>("device-path");
    let requested_metadata = requested.metadata().map_err(|error| {
        MediaGraphError::new(format!(
            "inspect selected render device {}: {error}",
            requested.display()
        ))
    })?;
    if !requested_metadata.file_type().is_char_device() {
        return Err(MediaGraphError::new(format!(
            "selected render device {} is not a character device",
            requested.display()
        )));
    }
    let actual_metadata = Path::new(&actual).metadata().map_err(|error| {
        MediaGraphError::new(format!(
            "inspect {actual} selected by {}: {error}",
            element.name()
        ))
    })?;
    if !actual_metadata.file_type().is_char_device()
        || requested_metadata.rdev() != actual_metadata.rdev()
    {
        return Err(MediaGraphError::new(format!(
            "{} selected {actual}, not {}",
            element.name(),
            requested.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_encoding_requires_its_system_memory_layout() {
        let encoder = VideoEncoder::software(VideoCodec::H264);
        assert!(encoder
            .validate_input(&VideoInputLayout::SystemMemoryBgrx)
            .is_ok());
        assert!(encoder
            .validate_input(&VideoInputLayout::DmaBuf {
                drm_format: DrmVideoFormat {
                    format: u32::from_le_bytes(*b"AR24"),
                    modifier: 0x0100_0000_0000_0009,
                },
            })
            .is_err());
    }

    #[test]
    fn va_h264_encoding_requires_dma_buf_input() {
        gst::init().unwrap();
        let encoder = VideoEncoder::va_h264("/dev/dri/renderD128");
        assert_eq!(encoder.codec(), VideoCodec::H264);
        assert!(encoder
            .validate_input(&VideoInputLayout::DmaBuf {
                drm_format: DrmVideoFormat {
                    format: u32::from_le_bytes(*b"AR24"),
                    modifier: 0x0100_0000_0000_0009,
                },
            })
            .is_ok());
        assert!(encoder
            .validate_input(&VideoInputLayout::SystemMemoryBgrx)
            .is_err());
        assert!(encoder
            .input_caps(VideoCadence::new(
                std::num::NonZeroU32::new(30_000).unwrap(),
                std::num::NonZeroU32::new(1_001).unwrap(),
            ))
            .unwrap()
            .to_string()
            .contains("memory:VAMemory"));
    }
}
