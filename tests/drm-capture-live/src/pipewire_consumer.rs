use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{ensure, Context};
use gstreamer::{self as gst, prelude::*};
use tokio::sync::mpsc;

pub struct Consumer {
    pipeline: gst::Pipeline,
    _socket: UnixStream,
    samples: mpsc::Receiver<gst::Sample>,
}

impl Consumer {
    pub fn start(socket: &Path, node: &str) -> anyhow::Result<Self> {
        gst::init()?;
        let socket = UnixStream::connect(socket)?;
        let pipeline = gst::parse::launch(
            "pipewiresrc name=source autoconnect=false stream-properties=props,node.name=pronk.capture-test-consumer ! video/x-raw,format=BGRx,width=640,height=480,framerate=30/1 ! appsink name=sink sync=false enable-last-sample=false"
        )?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("expected pipeline"))?;
        let source = pipeline.by_name("source").context("source")?;
        source.set_property("fd", socket.as_raw_fd());
        source.set_property("target-object", node);
        let sink = pipeline
            .by_name("sink")
            .context("sink")?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("expected appsink"))?;
        let (send, samples) = mpsc::channel(16);
        sink.set_callbacks(
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
            _socket: socket,
            samples,
        };
        owner.pipeline.set_state(gst::State::Playing)?;
        Ok(owner)
    }

    pub async fn next(&mut self) -> anyhow::Result<gst::Sample> {
        self.samples.recv().await.context("consumer stopped")
    }

    pub fn check(&self) -> anyhow::Result<()> {
        let bus = self.pipeline.bus().context("pipeline bus")?;
        while let Some(message) = bus.pop() {
            if let gst::MessageView::Error(error) = message.view() {
                anyhow::bail!("consumer error: {} ({:?})", error.error(), error.debug());
            }
        }
        Ok(())
    }
}

pub fn check_pixels(sample: &gst::Sample, expected: u8) -> anyhow::Result<u64> {
    let info = gstreamer_video::VideoInfo::from_caps(sample.caps().context("sample caps")?)?;
    ensure!(
        info.width() == 640
            && info.height() == 480
            && info.format() == gstreamer_video::VideoFormat::Bgrx,
        "unexpected sample layout"
    );
    let buffer = sample.buffer().context("sample buffer")?;
    ensure!(
        buffer.n_memory() > 0
            && buffer
                .iter_memories()
                .all(|memory| memory.is_type("dmabuf")),
        "consumer did not retain DMA-BUF memory"
    );
    let pixels = buffer.map_readable().context("map sample")?;
    let stride = usize::try_from(info.stride()[0])?;
    ensure!(
        stride >= 640 * 4 && pixels.len() >= stride * 480,
        "short sample"
    );
    for y in 0..480 {
        for x in 0..640 * 4 {
            ensure!(
                pixels[y * stride + x] == if x % 4 == 3 { 0xff } else { expected },
                "incorrect pixel in frame {} at {x},{y}",
                buffer.offset()
            );
        }
    }
    Ok(buffer.offset())
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
