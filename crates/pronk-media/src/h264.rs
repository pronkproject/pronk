use gstreamer as gst;

use crate::model::{
    MediaGraphError, VideoFrameDependency, MAX_ENCODED_ACCESS_UNIT_BYTES, VIDEO_FRAME_RATE,
};

pub(crate) const ENCODER_NAME: &str = "x264enc";
const KEY_FRAME_INTERVAL_SECONDS: u64 = 2;

pub(crate) fn encoder_input_caps() -> Result<gst::Caps, MediaGraphError> {
    format!("video/x-raw,format=(string)I420,framerate=(fraction){VIDEO_FRAME_RATE}/1")
        .parse::<gst::Caps>()
        .map_err(|error| MediaGraphError::new(format!("construct encoder input caps: {error}")))
}

pub(crate) fn encoder_output_caps() -> Result<gst::Caps, MediaGraphError> {
    "video/x-h264,stream-format=(string)byte-stream,alignment=(string)au,profile=(string)constrained-baseline"
        .parse::<gst::Caps>()
        .map_err(|error| MediaGraphError::new(format!("construct encoder output caps: {error}")))
}

pub(crate) fn bitrate_kbits(bits_per_second: u64) -> Result<u32, MediaGraphError> {
    const MAX_X264_BITRATE_KBITS: u64 = 2_048_000;

    let kbits = bits_per_second.div_ceil(1_000);
    if !(1..=MAX_X264_BITRATE_KBITS).contains(&kbits) {
        return Err(MediaGraphError::new(format!(
            "requested video bitrate {bits_per_second} bit/s is outside x264enc's supported range"
        )));
    }
    u32::try_from(kbits)
        .map_err(|_| MediaGraphError::new("x264enc bitrate does not fit its property type"))
}

pub(crate) fn key_frame_interval() -> u32 {
    let frames = u64::from(VIDEO_FRAME_RATE).saturating_mul(KEY_FRAME_INTERVAL_SECONDS);
    u32::try_from(frames.clamp(1, u64::from(u32::MAX))).unwrap_or(u32::MAX)
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
    if structure.name().as_str() != "video/x-h264" {
        return Err(MediaGraphError::new(format!(
            "encoder produced {}, not video/x-h264",
            structure.name()
        )));
    }
    for (field, expected) in [
        ("stream-format", "byte-stream"),
        ("alignment", "au"),
        ("profile", "constrained-baseline"),
    ] {
        let actual = structure.get::<&str>(field).map_err(|_| {
            MediaGraphError::new(format!("encoded video caps have no fixed {field}"))
        })?;
        if actual != expected {
            return Err(MediaGraphError::new(format!(
                "encoded video caps {field} is {actual:?}; expected {expected:?}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_access_unit(
    bytes: &[u8],
    dependency: VideoFrameDependency,
    first: bool,
) -> Result<(), MediaGraphError> {
    if bytes.is_empty() {
        return Err(MediaGraphError::new("encoded video access unit is empty"));
    }
    if bytes.len() > MAX_ENCODED_ACCESS_UNIT_BYTES {
        return Err(MediaGraphError::new(format!(
            "encoded video access unit is {} bytes; limit is {MAX_ENCODED_ACCESS_UNIT_BYTES}",
            bytes.len()
        )));
    }
    let nal_types = annex_b_nal_types(bytes)?;
    if first && dependency != VideoFrameDependency::KeyFrame {
        return Err(MediaGraphError::new(
            "the first encoded video access unit is not a key frame",
        ));
    }
    match dependency {
        VideoFrameDependency::KeyFrame => {
            for (nal_type, name) in [(7, "SPS"), (8, "PPS"), (5, "IDR slice")] {
                if !nal_types.contains(&nal_type) {
                    return Err(MediaGraphError::new(format!(
                        "H.264 key-frame access unit does not contain {name}"
                    )));
                }
            }
        }
        VideoFrameDependency::Delta if nal_types.contains(&5) => {
            return Err(MediaGraphError::new(
                "H.264 access unit contains an IDR slice but is marked as a delta frame",
            ));
        }
        VideoFrameDependency::Delta => {}
    }
    Ok(())
}

fn annex_b_nal_types(bytes: &[u8]) -> Result<Vec<u8>, MediaGraphError> {
    let Some((mut start, mut prefix_bytes)) = find_start_code(bytes, 0) else {
        return Err(MediaGraphError::new(
            "H.264 access unit has no Annex-B start code",
        ));
    };
    if bytes[..start].iter().any(|byte| *byte != 0) {
        return Err(MediaGraphError::new(
            "H.264 access unit has nonzero data before its first Annex-B start code",
        ));
    }

    let mut nal_types = Vec::new();
    loop {
        let payload = start + prefix_bytes;
        let next = find_start_code(bytes, payload);
        let end = next.map_or(bytes.len(), |(next_start, _)| next_start);
        if payload >= end {
            return Err(MediaGraphError::new(
                "H.264 access unit contains an empty Annex-B NAL unit",
            ));
        }
        let nal_type = bytes[payload] & 0x1f;
        if nal_type == 0 {
            return Err(MediaGraphError::new(
                "H.264 access unit contains an unspecified NAL unit type",
            ));
        }
        nal_types.push(nal_type);
        let Some((next_start, next_prefix_bytes)) = next else {
            break;
        };
        start = next_start;
        prefix_bytes = next_prefix_bytes;
    }
    Ok(nal_types)
}

fn find_start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..bytes.len()).find_map(|index| {
        if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            Some((index, 4))
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            Some((index, 3))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::VideoFrameDependency;

    use super::{annex_b_nal_types, validate_access_unit};

    #[test]
    fn annex_b_parser_accepts_mixed_prefixes_and_reports_nal_types() {
        let access_unit = [
            0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0, 0, 1, 0x65, 0xcc,
        ];
        assert_eq!(annex_b_nal_types(&access_unit).unwrap(), [7, 8, 5]);
        validate_access_unit(&access_unit, VideoFrameDependency::KeyFrame, true).unwrap();
    }

    #[test]
    fn access_unit_validation_rejects_broken_framing_and_dependencies() {
        assert!(annex_b_nal_types(&[0x67, 0xaa]).is_err());
        assert!(annex_b_nal_types(&[0xff, 0, 0, 1, 0x67]).is_err());
        assert!(annex_b_nal_types(&[0, 0, 1]).is_err());

        let delta = [0, 0, 0, 1, 0x41, 0xaa];
        assert!(validate_access_unit(&delta, VideoFrameDependency::Delta, true).is_err());

        let incomplete_key_frame = [0, 0, 0, 1, 0x65, 0xaa];
        assert!(
            validate_access_unit(&incomplete_key_frame, VideoFrameDependency::KeyFrame, false,)
                .is_err()
        );

        let mislabeled_idr = [0, 0, 0, 1, 0x65, 0xaa];
        assert!(
            validate_access_unit(&mislabeled_idr, VideoFrameDependency::Delta, false,).is_err()
        );
    }
}
