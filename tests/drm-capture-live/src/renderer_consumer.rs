use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{ensure, Context};
use gstreamer::{self as gst, prelude::*};
use tokio::sync::mpsc;

pub struct Consumer {
    pipeline: gst::Pipeline,
    _socket: UnixStream,
    buffers: mpsc::Receiver<gst::Buffer>,
}

impl Consumer {
    pub fn start(socket: &Path, node: &str, caps: &str) -> anyhow::Result<Self> {
        gst::init()?;
        let socket = UnixStream::connect(socket)?;
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc name=source autoconnect=false stream-properties=props,node.name=pronk.renderer-test-consumer ! {caps} ! appsink name=sink sync=false enable-last-sample=false"
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("expected pipeline"))?;
        let source = pipeline.by_name("source").context("source")?;
        source.set_property("fd", socket.as_raw_fd());
        source.set_property("target-object", node);
        let sink = pipeline
            .by_name("sink")
            .context("sink")?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("expected appsink"))?;
        let (send, buffers) = mpsc::channel(16);
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                    let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                    send.try_send(buffer).map_err(|_| gst::FlowError::Error)?;
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let owner = Self {
            pipeline,
            _socket: socket,
            buffers,
        };
        owner.pipeline.set_state(gst::State::Playing)?;
        Ok(owner)
    }

    pub async fn next(&mut self) -> anyhow::Result<gst::Buffer> {
        self.buffers.recv().await.context("consumer stopped")
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

pub fn check_buffer(buffer: &gst::BufferRef) -> anyhow::Result<u64> {
    ensure!(buffer.n_memory() > 0, "empty renderer buffer");
    ensure!(
        buffer
            .iter_memories()
            .all(|memory| memory.is_type("dmabuf")),
        "renderer consumer did not receive DMA-BUF memory"
    );
    Ok(buffer.offset())
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
