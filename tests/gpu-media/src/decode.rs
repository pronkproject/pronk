//! CPU readback is confined to this test oracle, after hardware encoding.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use gstreamer::{self as gst, prelude::*};
use gstreamer_video::{self as video, prelude::*};

use crate::pattern::{self, EDGE_TOLERANCE, FRAMES, TOLERANCE};
use crate::OutputSize;

struct Pipeline(gst::Pipeline);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

pub fn verify(
    frames: Vec<crate::encoded::Frame>,
    render_node: &Path,
    output_size: OutputSize,
) -> Result<()> {
    ensure!(
        !frames.is_empty() && frames.len() <= FRAMES as usize,
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
    let sequences = frames
        .iter()
        .map(|frame| frame.sequence)
        .collect::<Vec<_>>();
    for frame in frames {
        let mut buffer = gst::Buffer::from_mut_slice(frame.data);
        let buffer_ref = buffer.get_mut().context("unique encoded buffer")?;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(
            u64::from(frame.sequence) * 1_000_000_000 / 30,
        ));
        buffer_ref.set_duration(gst::ClockTime::from_nseconds(1_000_000_000 / 30));
        source.push_buffer(buffer)?;
    }
    source.end_of_stream()?;
    for (index, sequence) in sequences.into_iter().enumerate() {
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .context("missing decoded image")?;
        let info = video::VideoInfo::from_caps(sample.caps().context("decoded caps")?)?;
        ensure!(
            info.width() == output_size.width
                && info.height() == output_size.height
                && info.format() == video::VideoFormat::Bgra,
            "unexpected decoded layout"
        );
        let frame = video::VideoFrameRef::from_buffer_ref_readable(
            sample.buffer().context("decoded buffer")?,
            &info,
        )?;
        let stride = usize::try_from(frame.plane_stride()[0])?;
        let pixels = frame.plane_data(0)?;
        let regions = pattern::scene(sequence).map(|plane| {
            let region = plane.visible();
            let [left, top] = region.destination();
            (
                left..left + region.extent().width(),
                top..top + region.extent().height(),
                pattern::output_color(plane.color),
            )
        });
        let background = pattern::output_color(pattern::BACKGROUND);
        for y in 0..output_size.height as usize {
            let row = pixels
                .get(y * stride..y * stride + output_size.width as usize * 4)
                .context("short decoded plane")?;
            for (x, pixel) in row.chunks_exact(4).enumerate() {
                let expected = regions
                    .iter()
                    .rev()
                    .find_map(|(horizontal, vertical, color)| {
                        (horizontal.contains(&(x as u32)) && vertical.contains(&(y as u32)))
                            .then_some(*color)
                    })
                    .unwrap_or(background);
                let actual = [pixel[2], pixel[1], pixel[0]];
                let tolerance = if regions.iter().any(|(horizontal, vertical, _)| {
                    near_edge(horizontal, vertical, x as u32, y as u32)
                }) {
                    EDGE_TOLERANCE
                } else {
                    TOLERANCE
                };
                ensure!(
                    actual
                        .into_iter()
                        .zip(expected)
                        .all(|(a, b)| a.abs_diff(b) <= tolerance),
                    "decoded output {index} for frame {sequence} pixel {x},{y}: actual {actual:?}, expected {expected:?}"
                );
            }
        }
        eprintln!("decoded output={index} frame={sequence} pixels=match");
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

fn near_edge(
    horizontal: &std::ops::Range<u32>,
    vertical: &std::ops::Range<u32>,
    x: u32,
    y: u32,
) -> bool {
    const WIDTH: u32 = 2;
    let near_horizontal_extent =
        x >= horizontal.start.saturating_sub(WIDTH) && x < horizontal.end.saturating_add(WIDTH);
    let near_vertical_extent =
        y >= vertical.start.saturating_sub(WIDTH) && y < vertical.end.saturating_add(WIDTH);
    let near_vertical_edge =
        x.abs_diff(horizontal.start) <= WIDTH || x.abs_diff(horizontal.end) <= WIDTH;
    let near_horizontal_edge =
        y.abs_diff(vertical.start) <= WIDTH || y.abs_diff(vertical.end) <= WIDTH;
    (near_vertical_edge && near_vertical_extent) || (near_horizontal_edge && near_horizontal_extent)
}
