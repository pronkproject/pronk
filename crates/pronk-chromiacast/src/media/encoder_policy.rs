use std::path::PathBuf;

use pronk_backend_protocol::{RawVideoLayout, RenderDeviceIdentity};
use pronk_media::{MediaGraphError, VideoCodec, VideoEncoder};

use crate::transport::VideoOffer;

const SOFTWARE_RAW_LAYOUTS: [RawVideoLayout; 1] =
    [RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VideoEncoderPolicy {
    Software,
    VaH264 {
        render_node: PathBuf,
        render_device: RenderDeviceIdentity,
        raw_layouts: Vec<RawVideoLayout>,
        minimum_bitrate: u64,
        maximum_bitrate: u64,
    },
}

impl VideoEncoderPolicy {
    pub(super) fn raw_layouts(&self) -> &[RawVideoLayout] {
        match self {
            Self::Software => &SOFTWARE_RAW_LAYOUTS,
            Self::VaH264 { raw_layouts, .. } => raw_layouts,
        }
    }

    pub(super) fn minimum_bitrate(&self) -> u64 {
        match self {
            Self::Software => 0,
            Self::VaH264 {
                minimum_bitrate, ..
            } => *minimum_bitrate,
        }
    }

    pub(super) fn validate_bitrate(&self, bitrate: u64) -> Result<(), String> {
        let Self::VaH264 {
            minimum_bitrate: minimum,
            maximum_bitrate: maximum,
            ..
        } = self
        else {
            return Ok(());
        };
        if bitrate < *minimum {
            return Err(format!(
                "video bitrate {bitrate} bit/s is below the selected encoder minimum of {minimum} bit/s"
            ));
        }
        if bitrate > *maximum {
            return Err(format!(
                "video bitrate {bitrate} bit/s exceeds the selected encoder maximum of {maximum} bit/s"
            ));
        }
        Ok(())
    }

    pub(super) fn offer(&self) -> VideoOffer {
        match self {
            Self::Software => VideoOffer::H264Preferred,
            Self::VaH264 { .. } => VideoOffer::H264Only,
        }
    }

    pub(super) fn encoder(&self, codec: VideoCodec) -> Result<VideoEncoder, MediaGraphError> {
        match (self, codec) {
            (Self::Software, codec) => Ok(VideoEncoder::software(codec)),
            (Self::VaH264 { render_node, .. }, VideoCodec::H264) => {
                Ok(VideoEncoder::va_h264(render_node.clone()))
            }
            (Self::VaH264 { .. }, VideoCodec::Vp8) => Err(MediaGraphError::new(
                "the selected VA encoder does not support VP8",
            )),
        }
    }

    pub(super) fn validate_video_target(
        &self,
        render_device: Option<RenderDeviceIdentity>,
    ) -> Result<(), String> {
        let Self::VaH264 {
            render_device: selected,
            ..
        } = self
        else {
            return Ok(());
        };
        let Some(actual) = render_device else {
            return Err("the selected VA encoder requires a render-device identity".into());
        };
        if actual != *selected {
            return Err(format!(
                "video target uses render device {}:{}; the selected VA encoder uses {}:{}",
                actual.major, actual.minor, selected.major, selected.minor
            ));
        }
        Ok(())
    }
}
