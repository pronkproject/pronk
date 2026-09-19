use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::mpsc;

use crate::OutputSize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Raw,
    VaH264,
}

pub enum Event {
    Input(gst::Buffer),
    Encoded(gst::Sample),
    Error(String),
}

pub struct InputLayout {
    pub modifier: u64,
    pub fourcc: &'static str,
    pub size: OutputSize,
}

pub struct Consumer {
    pipeline: gst::Pipeline,
    // pipewiresrc duplicates this connection when entering READY.
    _socket: UnixStream,
    samples: mpsc::Receiver<Event>,
}

impl Consumer {
    pub fn start(
        socket: &Path,
        node: &str,
        input: InputLayout,
        frames: u32,
        render_node: &Path,
        mode: Mode,
    ) -> Result<Self> {
        gst::init()?;
        let socket = UnixStream::connect(socket)?;
        let drm_format = if input.modifier == 0 {
            input.fourcc.into()
        } else {
            format!("{}:0x{:016x}", input.fourcc, input.modifier)
        };
        let caps = format!(
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm_format},width={},height={},framerate=30/1",
            input.size.width, input.size.height
        );
        let encoding = match mode {
            Mode::Raw => "",
            Mode::VaH264 => "vapostproc name=convert ! video/x-raw(memory:VAMemory),format=NV12 ! vah264enc name=encoder b-frames=0 cabac=false dct8x8=false key-int-max=30 aud=true ! video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! ",
        };
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc name=source autoconnect=false num-buffers={frames} stream-properties=props,node.name=pronk.gpu-test-consumer ! {caps} ! {encoding}appsink name=sink sync=false enable-last-sample=false"
        ))?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("expected pipeline"))?;
        let source = pipeline.by_name("source").context("source element")?;
        source.set_property("fd", socket.as_raw_fd());
        source.set_property("target-object", node);
        let sink = pipeline
            .by_name("sink")
            .context("sink element")?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("expected appsink"))?;
        let (send, samples) = mpsc::channel(frames as usize * 2 + 1);
        if mode == Mode::VaH264 {
            for name in ["convert", "encoder"] {
                let element = pipeline.by_name(name).context("VA element")?;
                let path = element
                    .property::<Option<String>>("device-path")
                    .context("VA device path")?;
                ensure!(
                    std::fs::metadata(&path)?.rdev() == std::fs::metadata(render_node)?.rdev(),
                    "VA device {path} differs from the selected GPU"
                );
            }
            let input = send.clone();
            source
                .static_pad("src")
                .context("source pad")?
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                        if input.try_send(Event::Input(buffer.clone())).is_err() {
                            return gst::PadProbeReturn::Drop;
                        }
                    }
                    gst::PadProbeReturn::Ok
                })
                .context("install native input probe")?;
            let check = send.clone();
            let reported_caps = std::sync::atomic::AtomicBool::new(false);
            pipeline
                .by_name("convert")
                .unwrap()
                .static_pad("src")
                .context("conversion pad")?
                .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                    if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                        if !reported_caps.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            eprintln!("VA conversion caps: {:?}", pad.current_caps());
                        }
                        if buffer.n_memory() == 0
                            || !buffer
                                .iter_memories()
                                .all(|memory| memory.is_type("VAMemory"))
                        {
                            let _ = check.try_send(Event::Error(
                                "postprocessor did not produce VA memory".into(),
                            ));
                            return gst::PadProbeReturn::Drop;
                        }
                    }
                    gst::PadProbeReturn::Ok
                })
                .context("install VA output probe")?;
        }
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                    let event = match mode {
                        Mode::Raw => {
                            Event::Input(sample.buffer_owned().ok_or(gst::FlowError::Error)?)
                        }
                        Mode::VaH264 => Event::Encoded(sample),
                    };
                    send.try_send(event).map_err(|_| gst::FlowError::Error)?;
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

    pub async fn next(&mut self) -> Result<Event> {
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

pub fn sequence(buffer: &gst::BufferRef) -> Result<u64> {
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
