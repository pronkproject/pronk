use gstreamer as gst;

use crate::model::{
    MediaGraphError, VideoFrameDependency, MAX_ENCODED_ACCESS_UNIT_BYTES, VIDEO_FRAME_RATE,
};

pub(crate) const ENCODER_NAME: &str = "vp8enc";
const KEY_FRAME_INTERVAL_SECONDS: u64 = 2;

pub(crate) fn encoder_input_caps() -> Result<gst::Caps, MediaGraphError> {
    format!("video/x-raw,format=(string)I420,framerate=(fraction){VIDEO_FRAME_RATE}/1")
        .parse::<gst::Caps>()
        .map_err(|error| MediaGraphError::new(format!("construct encoder input caps: {error}")))
}

pub(crate) fn encoder_output_caps() -> Result<gst::Caps, MediaGraphError> {
    "video/x-vp8,profile=(string)0"
        .parse::<gst::Caps>()
        .map_err(|error| MediaGraphError::new(format!("construct encoder output caps: {error}")))
}

pub(crate) fn bitrate(bits_per_second: u64) -> Result<i32, MediaGraphError> {
    i32::try_from(bits_per_second).map_err(|_| {
        MediaGraphError::new(format!(
            "requested video bitrate {bits_per_second} bit/s is outside vp8enc's supported range"
        ))
    })
}

pub(crate) fn key_frame_interval() -> i32 {
    let frames = u64::from(VIDEO_FRAME_RATE).saturating_mul(KEY_FRAME_INTERVAL_SECONDS);
    i32::try_from(frames.clamp(1, i32::MAX as u64)).unwrap_or(i32::MAX)
}

pub(crate) fn validate_caps(caps: &gst::CapsRef) -> Result<(), MediaGraphError> {
    if caps.size() != 1 || !caps.is_fixed() {
        return Err(MediaGraphError::new(
            "encoded video caps must contain exactly one fixed structure",
        ));
    }
    let structure = caps
        .structure(0)
        .ok_or_else(|| MediaGraphError::new("encoded video caps have no structure"))?;
    if structure.name().as_str() != "video/x-vp8" {
        return Err(MediaGraphError::new(format!(
            "encoder produced {}, not video/x-vp8",
            structure.name()
        )));
    }
    let profile = structure
        .get::<&str>("profile")
        .map_err(|_| MediaGraphError::new("encoded video caps have no fixed profile"))?;
    if profile != "0" {
        return Err(MediaGraphError::new(format!(
            "encoded video caps profile is {profile:?}; expected \"0\""
        )));
    }
    Ok(())
}

pub(crate) fn validate_frame(
    bytes: &[u8],
    dependency: VideoFrameDependency,
    first: bool,
) -> Result<(), MediaGraphError> {
    if bytes.len() < 3 {
        return Err(MediaGraphError::new(
            "encoded VP8 frame is shorter than its frame tag",
        ));
    }
    if bytes.len() > MAX_ENCODED_ACCESS_UNIT_BYTES {
        return Err(MediaGraphError::new(format!(
            "encoded VP8 frame is {} bytes; limit is {MAX_ENCODED_ACCESS_UNIT_BYTES}",
            bytes.len()
        )));
    }

    let encoded_dependency = if bytes[0] & 1 == 0 {
        VideoFrameDependency::KeyFrame
    } else {
        VideoFrameDependency::Delta
    };
    if dependency != encoded_dependency {
        return Err(MediaGraphError::new(format!(
            "VP8 frame payload is {encoded_dependency:?} but its buffer is marked {dependency:?}"
        )));
    }
    if first && dependency != VideoFrameDependency::KeyFrame {
        return Err(MediaGraphError::new(
            "the first encoded video frame is not a key frame",
        ));
    }
    if dependency == VideoFrameDependency::KeyFrame
        && (bytes.len() < 10 || bytes[3..6] != [0x9d, 0x01, 0x2a])
    {
        return Err(MediaGraphError::new(
            "VP8 key frame does not contain the uncompressed start code",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::VideoFrameDependency;

    use super::validate_frame;

    #[test]
    fn frame_validation_checks_payload_dependency_and_key_frame_header() {
        let key_frame = [0x10, 0, 0, 0x9d, 0x01, 0x2a, 0, 0, 0, 0];
        validate_frame(&key_frame, VideoFrameDependency::KeyFrame, true).unwrap();

        let delta_frame = [0x11, 0, 0];
        validate_frame(&delta_frame, VideoFrameDependency::Delta, false).unwrap();
        assert!(validate_frame(&delta_frame, VideoFrameDependency::Delta, true).is_err());
        assert!(validate_frame(&delta_frame, VideoFrameDependency::KeyFrame, false).is_err());
        assert!(validate_frame(&key_frame[..6], VideoFrameDependency::KeyFrame, false).is_err());
    }
}
