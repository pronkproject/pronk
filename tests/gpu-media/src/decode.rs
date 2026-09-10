//! CPU readback is confined to this test oracle, after hardware encoding.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use gstreamer::{self as gst, prelude::*};
use gstreamer_video::{self as video, prelude::*};

use crate::pattern::{self, color, FRAMES, HEIGHT, TOLERANCE, WIDTH};

struct Pipeline(gst::Pipeline);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

pub fn verify(frames: Vec<Vec<u8>>, render_node: &Path) -> Result<()> {
    ensure!(
        frames.len() == FRAMES as usize,
        "unexpected fixture frame count"
    );
    let pipeline = Pipeline(gst::parse::launch(
        "appsrc name=source format=time ! h264parse ! vah264dec name=decoder ! vapostproc name=convert interpolation-method=nearest-neighbor ! video/x-raw,format=BGRA ! appsink name=sink sync=false enable-last-sample=false",
    )?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("decode pipeline"))?);
    for name in ["decoder", "convert"] {
        let path = pipeline
            .0
            .by_name(name)
            .context("VA decode element")?
            .property::<Option<String>>("device-path")
            .context("VA decode device")?;
        ensure!(
            std::fs::metadata(path)?.rdev() == std::fs::metadata(render_node)?.rdev(),
            "decode GPU differs from selected render node"
        );
    }
    let source = pipeline
        .0
        .by_name("source")
        .context("decode source")?
        .downcast::<gstreamer_app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("decode appsrc"))?;
    source.set_caps(Some(
        &gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    ));
    let sink = pipeline
        .0
        .by_name("sink")
        .context("decode sink")?
        .downcast::<gstreamer_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("decode appsink"))?;
    pipeline.0.set_state(gst::State::Playing)?;
    let count = frames.len();
    for (index, bytes) in frames.into_iter().enumerate() {
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let buffer_ref = buffer.get_mut().context("unique encoded buffer")?;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(
            index as u64 * 1_000_000_000 / 30,
        ));
        buffer_ref.set_duration(gst::ClockTime::from_nseconds(1_000_000_000 / 30));
        source.push_buffer(buffer)?;
    }
    source.end_of_stream()?;
    for index in 0..count {
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .context("missing decoded image")?;
        let info = video::VideoInfo::from_caps(sample.caps().context("decoded caps")?)?;
        ensure!(
            info.width() == WIDTH
                && info.height() == HEIGHT
                && info.format() == video::VideoFormat::Bgra,
            "unexpected decoded layout"
        );
        let frame = video::VideoFrameRef::from_buffer_ref_readable(
            sample.buffer().context("decoded buffer")?,
            &info,
        )?;
        let stride = usize::try_from(frame.plane_stride()[0])?;
        let pixels = frame.plane_data(0)?;
        let foreground = color(index as u32);
        let background = pattern::BACKGROUND;
        let region = pattern::visible(index as u32);
        let [left, top] = region.destination();
        let right = left + region.extent().width();
        let bottom = top + region.extent().height();
        for y in 0..HEIGHT as usize {
            let row = pixels
                .get(y * stride..y * stride + WIDTH as usize * 4)
                .context("short decoded plane")?;
            for (x, pixel) in row.chunks_exact(4).enumerate() {
                let visible =
                    (left..right).contains(&(x as u32)) && (top..bottom).contains(&(y as u32));
                let expected = if visible { foreground } else { background };
                let actual = [pixel[2], pixel[1], pixel[0]];
                ensure!(
                    actual
                        .into_iter()
                        .zip(expected)
                        .all(|(a, b)| a.abs_diff(b) <= TOLERANCE),
                    "decoded frame {index} pixel {x},{y}: actual {actual:?}, expected {expected:?}"
                );
            }
        }
        eprintln!("decoded frame={index} pixels=match");
    }
    ensure!(
        sink.try_pull_sample(gst::ClockTime::from_seconds(5))
            .is_none()
            && sink.is_eos(),
        "extra image or missing decoder EOS"
    );
    let bus = pipeline.0.bus().context("decode bus")?;
    while let Some(message) = bus.pop() {
        if let gst::MessageView::Error(error) = message.view() {
            anyhow::bail!("decoder error: {} ({:?})", error.error(), error.debug());
        }
    }
    eprintln!("PASS: {count} decoded images match the ordered color sequence");
    Ok(())
}
