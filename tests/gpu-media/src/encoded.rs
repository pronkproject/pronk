use anyhow::{ensure, Context, Result};
use gstreamer as gst;

#[derive(Default)]
pub struct Encoded {
    frames: Vec<Vec<u8>>,
    pts: Option<gst::ClockTime>,
    origin: Option<gst::ClockTime>,
}

impl Encoded {
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
        let mut types = 0u32;
        for chunk in bytes.windows(4) {
            if chunk[..3] == [0, 0, 1] {
                types |= 1 << (chunk[3] & 31);
            }
        }
        if self.frames.is_empty() || !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            ensure!(
                types & ((1 << 5) | (1 << 7) | (1 << 8)) == ((1 << 5) | (1 << 7) | (1 << 8)),
                "keyframe lacks IDR/SPS/PPS"
            );
        }
        self.pts = Some(pts);
        self.origin = Some(origin);
        eprintln!(
            "encoded frame={} bytes={} pts={pts}",
            self.frames.len(),
            bytes.len()
        );
        self.frames.push(bytes.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(encoded.len(), 2);
    }
}
