use anyhow::{ensure, Context, Result};
use gstreamer as gst;
use pronk_media::{EncodedVideoAccessUnit, VideoFrameDependency};
use std::num::NonZeroU64;

#[derive(Default)]
pub struct Encoded {
    frames: Vec<Frame>,
    pts: Option<gst::ClockTime>,
    origin: Option<gst::ClockTime>,
}

pub struct Frame {
    pub sequence: u32,
    pub data: Vec<u8>,
}

impl Encoded {
    pub fn into_frames(self) -> Vec<Frame> {
        self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn push(&mut self, sample: &gst::Sample) -> Result<()> {
        let caps = sample.caps().context("encoded caps")?;
        let structure = caps.structure(0).context("encoded structure")?;
        ensure!(structure.name() == "video/x-h264", "not H.264 output");
        for (key, value) in [
            ("profile", "constrained-baseline"),
            ("stream-format", "byte-stream"),
            ("alignment", "au"),
        ] {
            ensure!(
                structure.get::<&str>(key)? == value,
                "unexpected H.264 {key}"
            );
        }
        let buffer = sample.buffer().context("encoded buffer")?;
        let pts = buffer.pts().context("encoded PTS")?;
        let dts = buffer.dts().context("encoded DTS")?;
        ensure!(
            dts <= pts && self.pts.is_none_or(|previous| previous < pts),
            "nonmonotonic encoded timing"
        );
        let origin = self.origin.unwrap_or(pts);
        ensure!(
            pts.nseconds().checked_sub(origin.nseconds())
                == Some(self.frames.len() as u64 * 1_000_000_000 / 30),
            "encoded timing does not preserve the fixture cadence"
        );
        let bytes = buffer.map_readable()?;
        ensure!(!bytes.is_empty(), "empty encoded frame");
        validate_h264(
            &bytes,
            self.frames.is_empty() || !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
        )?;
        self.pts = Some(pts);
        self.origin = Some(origin);
        eprintln!(
            "encoded frame={} bytes={} pts={pts}",
            self.frames.len(),
            bytes.len()
        );
        self.frames.push(Frame {
            sequence: self.frames.len().try_into()?,
            data: bytes.to_vec(),
        });
        Ok(())
    }

    pub fn push_access_unit(
        &mut self,
        unit: EncodedVideoAccessUnit,
        generation: NonZeroU64,
    ) -> Result<()> {
        let timestamp = unit.media_timestamp.as_nanos();
        let sequence = timestamp.saturating_mul(30).saturating_add(500_000_000) / 1_000_000_000;
        let sequence = u32::try_from(sequence)?;
        ensure!(
            unit.media_generation == generation,
            "production frame belongs to another media generation"
        );
        ensure!(
            self.frames
                .last()
                .is_none_or(|previous| previous.sequence < sequence),
            "production sequence did not advance"
        );
        let expected_timestamp = u128::from(sequence) * 1_000_000_000 / 30;
        ensure!(
            unit.media_timestamp.as_nanos().abs_diff(expected_timestamp) <= 1,
            "production timestamp {:?} differs from fixture frame {sequence}",
            unit.media_timestamp
        );
        let expected_duration = 1_000_000_000_u128 / 30;
        ensure!(
            unit.duration.as_nanos().abs_diff(expected_duration) <= 1,
            "production frame has an unexpected duration"
        );
        let key = unit.dependency == VideoFrameDependency::KeyFrame;
        ensure!(
            !self.frames.is_empty() || key,
            "production stream does not begin with a key frame"
        );
        validate_h264(&unit.data, key)?;
        eprintln!(
            "production encoded frame={sequence} bytes={} timestamp={:?}",
            unit.data.len(),
            unit.media_timestamp
        );
        self.frames.push(Frame {
            sequence,
            data: unit.data,
        });
        Ok(())
    }
}

fn validate_h264(bytes: &[u8], key: bool) -> Result<()> {
    ensure!(!bytes.is_empty(), "empty encoded frame");
    let mut types = 0u32;
    for chunk in bytes.windows(4) {
        if chunk[..3] == [0, 0, 1] {
            types |= 1 << (chunk[3] & 31);
        }
    }
    if key {
        ensure!(
            types & ((1 << 5) | (1 << 7) | (1 << 8)) == ((1 << 5) | (1 << 7) | (1 << 8)),
            "keyframe lacks IDR/SPS/PPS"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sample(bytes: &[u8], pts: u64, key: bool) -> gst::Sample {
        gst::init().unwrap();
        let mut buffer = gst::Buffer::from_mut_slice(bytes.to_vec());
        let data = buffer.get_mut().unwrap();
        data.set_pts(gst::ClockTime::from_nseconds(pts));
        data.set_dts(gst::ClockTime::from_nseconds(pts));
        if !key {
            data.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        let caps = gst::Caps::builder("video/x-h264")
            .field("profile", "constrained-baseline")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        gst::Sample::builder().buffer(&buffer).caps(&caps).build()
    }

    const KEY: &[u8] = &[0, 0, 0, 1, 7, 0, 0, 1, 8, 0, 0, 1, 5];

    fn access_unit(
        generation: u64,
        dependency: VideoFrameDependency,
        bytes: &[u8],
        timestamp: u64,
        duration: u64,
    ) -> EncodedVideoAccessUnit {
        EncodedVideoAccessUnit {
            media_generation: NonZeroU64::new(generation).unwrap(),
            dependency,
            data: bytes.to_vec(),
            media_timestamp: Duration::from_nanos(timestamp),
            reference_time: Instant::now(),
            duration: Duration::from_nanos(duration),
        }
    }

    #[test]
    fn fixture_validation_keeps_failed_frames_out_of_the_sequence() {
        let mut encoded = Encoded::default();
        assert!(encoded.push(&sample(&[], 100, true)).is_err());
        assert!(encoded.push(&sample(&[0, 0, 1, 5], 100, true)).is_err());
        assert_eq!(encoded.len(), 0);
        encoded.push(&sample(KEY, 100, true)).unwrap();
        assert!(encoded.push(&sample(KEY, 100, true)).is_err());
        assert!(encoded.push(&sample(KEY, 200, true)).is_err());
        assert_eq!(encoded.len(), 1);
        encoded
            .push(&sample(&[0, 0, 1, 1], 33_333_433, false))
            .unwrap();
        assert_eq!(encoded.into_frames().len(), 2);
    }

    #[test]
    fn production_validation_requires_one_generation_and_fixture_cadence() {
        let generation = NonZeroU64::new(7).unwrap();
        for invalid in [
            access_unit(8, VideoFrameDependency::KeyFrame, KEY, 0, 33_333_333),
            access_unit(7, VideoFrameDependency::Delta, KEY, 0, 33_333_333),
            access_unit(7, VideoFrameDependency::KeyFrame, KEY, 2, 33_333_333),
            access_unit(7, VideoFrameDependency::KeyFrame, KEY, 0, 33_333_335),
        ] {
            let mut encoded = Encoded::default();
            assert!(encoded.push_access_unit(invalid, generation).is_err());
            assert_eq!(encoded.len(), 0);
        }

        let mut encoded = Encoded::default();
        encoded
            .push_access_unit(
                access_unit(7, VideoFrameDependency::KeyFrame, KEY, 0, 33_333_334),
                generation,
            )
            .unwrap();
        encoded
            .push_access_unit(
                access_unit(
                    7,
                    VideoFrameDependency::Delta,
                    &[0, 0, 1, 1],
                    33_333_333,
                    33_333_333,
                ),
                generation,
            )
            .unwrap();
        assert_eq!(encoded.into_frames().len(), 2);
    }
}
