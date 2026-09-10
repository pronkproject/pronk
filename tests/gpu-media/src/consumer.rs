use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::mpsc;

pub struct Consumer {
    pipeline: gst::Pipeline,
    // pipewiresrc duplicates this connection when entering READY.
    _socket: UnixStream,
    samples: mpsc::Receiver<gst::Sample>,
}

impl Consumer {
    pub fn start(socket: &Path, node: &str, modifier: u64, frames: u32) -> Result<Self> {
        gst::init()?;
        let socket = UnixStream::connect(socket)?;
        let drm_format = if modifier == 0 {
            "XR24".into()
        } else {
            format!("XR24:0x{modifier:016x}")
        };
        let caps = format!(
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm_format},width=1920,height=1080,framerate=30/1"
        );
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc name=source autoconnect=false num-buffers={frames} stream-properties=props,node.name=pronk.gpu-test-consumer ! {caps} ! appsink name=sink sync=false enable-last-sample=false"
        ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("expected pipeline"))?;
        let source = pipeline.by_name("source").context("source element")?;
        source.set_property("fd", socket.as_raw_fd());
        source.set_property("target-object", node);
        let sink = pipeline
            .by_name("sink")
            .context("sink element")?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("expected appsink"))?;
        let (send, samples) = mpsc::channel(frames as usize);
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

    pub async fn next(&mut self) -> Result<gst::Sample> {
        self.samples
            .recv()
            .await
            .context("consumer sample channel closed")
    }

    pub fn check(&self) -> Result<()> {
        let bus = self.pipeline.bus().context("pipeline bus")?;
        while let Some(message) = bus.pop() {
            if let gst::MessageView::Error(error) = message.view() {
                anyhow::bail!("consumer error: {} ({:?})", error.error(), error.debug());
            }
        }
        Ok(())
    }
}

pub fn sequence(sample: &gst::Sample) -> Result<u64> {
    let buffer = sample.buffer().context("sample buffer")?;
    ensure!(buffer.n_memory() > 0, "empty raw buffer");
    ensure!(
        buffer
            .iter_memories()
            .all(|memory| memory.is_type("dmabuf")),
        "consumer did not receive DMA-BUF memory"
    );
    Ok(buffer.offset())
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
