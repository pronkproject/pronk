use anyhow::{ensure, Context};
use gstreamer::{self as gst, prelude::*};
use pronk_media::EncodedVideoAccessUnit;
use tokio::sync::mpsc;

pub struct Decoder {
    pipeline: gst::Pipeline,
    input: gstreamer_app::AppSrc,
    output: mpsc::Receiver<gst::Sample>,
}

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        gst::init()?;
        let pipeline = gst::parse::launch("appsrc name=input format=time ! video/x-h264,stream-format=byte-stream,alignment=au ! h264parse ! avdec_h264 ! videoconvert ! video/x-raw,format=BGRx ! appsink name=output sync=false enable-last-sample=false")?
            .downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("expected decode pipeline"))?;
        let input = pipeline
            .by_name("input")
            .context("decoder input")?
            .downcast::<gstreamer_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("expected appsrc"))?;
        input.set_caps(Some(
            &gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ));
        let output = pipeline
            .by_name("output")
            .context("decoder output")?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("expected appsink"))?;
        let (send, receive) = mpsc::channel(16);
        output.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                    send.try_send(sample).map_err(|_| gst::FlowError::Error)?;
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let owner = Self {
            pipeline,
            input,
            output: receive,
        };
        owner.pipeline.set_state(gst::State::Playing)?;
        Ok(owner)
    }

    pub fn push(&self, frame: EncodedVideoAccessUnit) -> anyhow::Result<()> {
        self.check()?;
        let mut buffer = gst::Buffer::from_mut_slice(frame.data);
        let writable = buffer.get_mut().context("unique encoded buffer")?;
        writable.set_pts(gst::ClockTime::from_nseconds(
            frame.media_timestamp.as_nanos().try_into()?,
        ));
        writable.set_duration(gst::ClockTime::from_nseconds(
            frame.duration.as_nanos().try_into()?,
        ));
        self.input.push_buffer(buffer)?;
        Ok(())
    }

    pub fn check(&self) -> anyhow::Result<()> {
        let bus = self.pipeline.bus().context("decoder bus")?;
        while let Some(message) = bus.pop() {
            if let gst::MessageView::Error(error) = message.view() {
                anyhow::bail!("decoder error: {} ({:?})", error.error(), error.debug());
            }
        }
        Ok(())
    }

    pub async fn next(&mut self, width: u32, height: u32) -> anyhow::Result<u8> {
        let sample = self.output.recv().await.context("decoder stopped")?;
        let info = gstreamer_video::VideoInfo::from_caps(sample.caps().context("decoded caps")?)?;
        ensure!(
            width > 0
                && height > 0
                && info.width() == width
                && info.height() == height
                && info.format() == gstreamer_video::VideoFormat::Bgrx,
            "decoded layout"
        );
        let buffer = sample.buffer().context("decoded pixels")?.map_readable()?;
        let stride = usize::try_from(info.stride()[0])?;
        let width = usize::try_from(width)?;
        let height = usize::try_from(height)?;
        let row = width.checked_mul(4).context("decoded row size overflow")?;
        let extent = stride
            .checked_mul(height)
            .context("decoded image size overflow")?;
        ensure!(
            stride >= row && buffer.len() >= extent,
            "short decoded image"
        );
        let value = if buffer[0].abs_diff(0x49) <= 4 {
            0x49
        } else {
            0x68
        };
        for y in 0..height {
            for x in 0..width {
                for channel in 0..3 {
                    ensure!(
                        buffer[y * stride + x * 4 + channel].abs_diff(value) <= 4,
                        "decoded content mismatch at ({x}, {y}) channel {channel}: got {}, expected {value}",
                        buffer[y * stride + x * 4 + channel]
                    );
                }
            }
        }
        Ok(value)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
