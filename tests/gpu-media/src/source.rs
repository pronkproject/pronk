use std::collections::{HashSet, VecDeque};
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use drm_display_executor::scheduler::source_use::{ClosedUse, SourceUse};
use futures_util::stream::{FuturesUnordered, StreamExt};
use pronk::gpu_output::{GpuOutput, OutputEvent, OutputReady};
use pronk_dmabuf::Completion;
use pronk_gpu::output_pool::OutputPool;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{
    PipeWireRemote, VideoBuffer, VideoBufferLayout, VideoBufferStorage, VideoDamage, VideoFrame,
    VideoSourceActor, VideoSourceConfig, VideoSourceGeneration,
};

use crate::consumer::{self, Consumer, Event, Mode};
use crate::encoded::Encoded;
use crate::pattern::{self, FRAMES, HEIGHT, WIDTH};
use crate::render;

const SLOTS: usize = 4;

pub async fn run(socket: &Path, node: &Path, modifier: u64, mode: Mode) -> Result<()> {
    ensure!(
        std::env::var_os("PIPEWIRE_REMOTE").as_deref() == Some(socket.as_os_str()),
        "development remote must match the explicitly supplied private socket"
    );
    let render_node = node.to_owned();
    let node = node.to_owned();
    // Allocation and native initialization run outside the runtime and PW loop.
    let (worker, mut images, staging, incoming) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let device = Arc::new(Device::open(&node)?);
            let producer = Device::open(&node)?;
            let identity = device.identity();
            ensure!(
                identity == producer.identity()
                    && identity.device != [0; 16]
                    && identity.driver != [0; 16],
                "producer and worker native identities do not match"
            );
            eprintln!("GPU: {}", device.name());
            let images = (0..SLOTS)
                .map(|_| {
                    device
                        .allocate(nz(WIDTH), nz(HEIGHT), modifier)
                        .map(Some)
                        .map_err(Into::into)
                })
                .collect::<Result<Vec<_>>>()?;
            let staging = render::PrivateStorage::allocate(&device)?;
            let incoming = pattern::scene(0)
                .into_iter()
                .map(|plane| {
                    producer.allocate(
                        nz(plane.crop.image().width()),
                        nz(plane.crop.image().height()),
                        modifier,
                    )
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            Ok((device, images, staging, incoming))
        })
        .await??;
    let mut staging = Some(staging);
    let mut incoming = Some(incoming);
    let mut exports = Vec::new();
    let mut buffers = Vec::new();
    let ids: Vec<_> = (1..=SLOTS as u32).map(nz).collect();
    for (image, id) in images.iter().zip(&ids) {
        let image = image.as_ref().unwrap();
        let layout = image.layout();
        exports.push(image.export()?);
        buffers.push(VideoBuffer {
            id: *id,
            dma_buf: image.export()?,
            timelines: None,
            layout: VideoBufferLayout {
                format: match mode {
                    Mode::Raw => pronk_pipewire::VideoPixelFormat::Xrgb8888,
                    Mode::VaH264 => pronk_pipewire::VideoPixelFormat::Argb8888,
                },
                width: layout.width,
                height: layout.height,
                pitch: NonZeroU32::new(layout.pitch.try_into()?).context("zero pitch")?,
                size: NonZeroU64::new(layout.allocation_size).context("zero allocation")?,
                storage: VideoBufferStorage::DrmModifier {
                    modifier: layout.modifier,
                    offset: layout.offset.try_into()?,
                },
            },
        });
    }
    let pool = OutputPool::new(exports)?;
    let mut actor = VideoSourceActor::spawn()?;
    let generation = NonZeroU64::new(1).unwrap();
    let identity = actor
        .start(VideoSourceGeneration {
            config: VideoSourceConfig {
                node_name: format!("pronk.gpu-test.{}", std::process::id()),
                node_description: "Pronk generated GPU test".into(),
                session_id: "private-test".into(),
                device_instance: "generated-gpu".into(),
                connector_id: nz(1),
                output_index: 0,
                grant_id: nz(1),
                media_generation: generation,
                refresh_hz: nz(30),
            },
            buffers,
            remote: PipeWireRemote::AmbientDevelopment,
        })
        .await?;
    let mut output = GpuOutput::new(identity.clone(), pool, ids)?;
    let link = tokio::process::Command::new("pw-link")
        .arg("--wait")
        .arg("--remote")
        .arg(socket)
        .arg(format!("{}:capture_1", identity.node_name))
        .arg("pronk.gpu-test-consumer:input_1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("link private video ports")?;
    let consumer_socket = socket.to_owned();
    let consumer_name = identity.node_name.clone();
    let consumer_render_node = render_node.clone();
    let mut consumer = tokio::task::spawn_blocking(move || {
        Consumer::start(
            &consumer_socket,
            &consumer_name,
            modifier,
            FRAMES,
            &consumer_render_node,
            mode,
        )
    })
    .await??;
    let link = link.wait_with_output().await?;
    ensure!(
        link.status.success(),
        "private link failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let mut waits = FuturesUnordered::new();
    let mut writable: VecDeque<NonZeroU32> = VecDeque::new();
    let mut published = 0;
    let mut received = HashSet::new();
    let mut held: Option<gstreamer::Buffer> = None;
    let mut encoded = Encoded::default();
    let mut uses = [0u32; SLOTS];
    let mut timing = render::Report::default();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    while received.len() < FRAMES as usize
        || (mode == Mode::VaH264 && encoded.len() < FRAMES as usize)
    {
        if published < FRAMES {
            if let Some(id) = writable.pop_front() {
                let slot = id.get() as usize - 1;
                let permit = output.claim(id)?;
                let image = images[slot].take().context("missing writable image")?;
                let private = staging
                    .take()
                    .context("private staging image is in flight")?;
                let input = incoming.take().context("producer image is in flight")?;
                let worker = Arc::clone(&worker);
                let scene = pattern::scene(published);
                let source_use = SourceUse::new(NonZeroUsize::new(scene.len()).unwrap())?;
                let submissions = (0..scene.len())
                    .map(|_| source_use.begin())
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let (submitted, ready) = tokio::sync::oneshot::channel();
                let (resume, collect) = std::sync::mpsc::channel();
                let read = tokio::task::spawn_blocking(move || {
                    let stage =
                        render::submit_sources(&worker, input, private, scene, submissions)?;
                    submitted
                        .send(())
                        .map_err(|_| anyhow::anyhow!("source coordinator closed"))?;
                    collect
                        .recv()
                        .context("source accounting was not collected")?;
                    stage.copy_output(image)
                });
                source_use.close();
                if ready.await.is_err() {
                    read.await??;
                    anyhow::bail!("source worker did not report submission");
                }
                let ClosedUse::Released(records) = source_use.finish()? else {
                    anyhow::bail!("source submission accounting failed");
                };
                ensure!(
                    records.len() == scene.len(),
                    "source use lacks a submitted native read completion"
                );
                resume
                    .send(())
                    .context("source worker closed before retirement")?;
                let rendered = read.await??;
                timing.push(rendered.timing);
                for record in records.iter().flatten() {
                    ensure!(
                        record.completion()? == Some(Completion::Success),
                        "retired source read did not complete successfully"
                    );
                }
                drop(records);
                incoming = Some(rendered.originals);
                staging = Some(rendered.private);
                images[slot] = Some(rendered.output);
                let pending = output.submitted(permit, rendered.completion)?;
                let OutputReady::Publish(permit) = output.complete(pending.wait().await)? else {
                    anyhow::bail!("missing publish permit")
                };
                let frame = output.begin_publish(
                    permit,
                    VideoFrame {
                        buffer_id: id,
                        sequence: u64::from(published),
                        pts_ns: i64::from(published) * 1_000_000_000 / 30,
                        damage: VideoDamage {
                            x: 0,
                            y: 0,
                            width: nz(WIDTH),
                            height: nz(HEIGHT),
                        },
                        discontinuity: published == 0,
                        acquire_point: None,
                    },
                )?;
                actor.publish(generation, frame).await?;
                published += 1;
                uses[slot] += 1;
            }
        }
        tokio::select! {
            event = actor.next_event() => {
                let event = event.context("source event stream closed")?;
                if let pronk_pipewire::VideoSourceActorEvent::BufferReleased { sequence, .. } = &event {
                    ensure!(received.contains(sequence), "release before consumer disposed its sample");
                    if let Some(sample) = &held {
                        ensure!(consumer::sequence(sample)? != *sequence, "release while sample is retained");
                    }
                }
                match output.handle_event(&event)? {
                    OutputEvent::Wait(wait) => waits.push(wait.wait()),
                    OutputEvent::Ignored => {},
                    OutputEvent::Stopped(_) => anyhow::bail!("source generation failed"),
                }
            }
            Some(finished) = waits.next(), if !waits.is_empty() => {
                match output.complete(finished)? {
                    OutputReady::Writable(id) => writable.push_back(id),
                    OutputReady::Publish(_) => anyhow::bail!("unexpected publication"),
                }
            }
            event = consumer.next() => {
                match event? {
                    Event::Input(buffer) => {
                        let seq = consumer::sequence(&buffer)?;
                        ensure!(seq == received.len() as u64, "out-of-order source sequence {seq}");
                        ensure!(seq < u64::from(FRAMES) && received.insert(seq), "duplicate or invalid source sequence {seq}");
                        if held.is_none() && received.len() == 1 { held = Some(buffer); }
                        if mode == Mode::Raw && received.len() == 6 { held = None; }
                    }
                    Event::Encoded(sample) => {
                        encoded.push(&sample)?;
                        if encoded.len() == 6 { held = None; }
                    }
                    Event::Error(error) => anyhow::bail!(error),
                }
            }
            _ = tick.tick() => consumer.check()?,
        }
    }
    consumer.check()?;
    drop(held);
    drop(consumer);
    let report = actor.stop(generation).await?;
    let retirement = output.stopped(&report)?;
    ensure!(
        retirement.errors.is_empty(),
        "native retirement snapshot failed"
    );
    for wait in retirement.waits {
        waits.push(wait.wait());
    }
    while let Some(done) = waits.next().await {
        output.complete(done)?;
    }
    actor.shutdown().await?;
    ensure!(
        uses.iter().all(|count| *count > 1),
        "not every image was rewritten: {uses:?}"
    );
    eprintln!("PASS: {published} generated frames, {} encoded, per-slot uses {uses:?}, first input retained through six outputs", encoded.len());
    timing.print();
    if mode == Mode::VaH264 {
        tokio::task::spawn_blocking(move || {
            crate::decode::verify(encoded.into_frames(), &render_node)
        })
        .await??;
    }
    Ok(())
}

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}
