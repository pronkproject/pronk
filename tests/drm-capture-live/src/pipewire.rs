//! Live reference capture through a private PipeWire server and GStreamer.

#[allow(dead_code)]
mod fixture;
mod pipewire_consumer;

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_capture::{allocation::Heap, CaptureError, Config, Layout, Session};
use pronk_capture_pipewire::Registration;
use pronk_pipewire::{
    PipeWireRemote, VideoSourceActor, VideoSourceActorEvent, VideoSourceConfig,
    VideoSourceGeneration,
};

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}
fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let device = PathBuf::from(args.next().context("expected unused VM DRM device")?);
    let socket = PathBuf::from(args.next().context("expected private PipeWire socket")?);
    ensure!(args.next().is_none(), "expected device and socket only");
    tokio::time::timeout(Duration::from_secs(30), run(&device, &socket)).await??;
    println!(
        "PASS: changing kernel captures through private PipeWire to GStreamer, with held sample"
    );
    Ok(())
}

async fn run(device: &Path, socket: &Path) -> anyhow::Result<()> {
    let mut fixture = fixture::Fixture::open(device)?;
    let heap = Heap::open(Path::new("/dev/dma_heap/system"))?;
    let (client, control) = drm_capture::create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let capture = Session::new(client).spawn(
        heap.allocate(
            Layout {
                width: nz(640),
                height: nz(480),
            },
            nz(3),
            nz64(16 * 1024 * 1024),
        )?,
        Config {
            capacity: nz(3),
            poll_interval: Duration::from_millis(2),
            shutdown_timeout: Duration::from_secs(5),
        },
    )?;
    let registration = Registration::new(&capture)?;
    let mut source = VideoSourceActor::spawn()?;
    let identity = source
        .start(VideoSourceGeneration {
            config: VideoSourceConfig {
                node_name: "pronk.capture-test".into(),
                node_description: "Kernel capture test".into(),
                session_id: "private-test".into(),
                device_instance: "castkms-test".into(),
                connector_id: nz(fixture.connector()),
                output_index: 0,
                media_generation: nz64(1),
                refresh_hz: nz(30),
            },
            buffers: registration.export()?,
            remote: PipeWireRemote::AmbientDevelopment,
        })
        .await?;
    let mut output = registration.bind(identity.clone());
    let mut link = tokio::process::Command::new("pw-link")
        .arg("--wait")
        .arg("--remote")
        .arg(socket)
        .arg(format!("{}:capture_1", identity.node_name))
        .arg("pronk.capture-test-consumer:input_1")
        .kill_on_drop(true)
        .spawn()
        .context("start private port link")?;
    let mut consumer = pipewire_consumer::Consumer::start(socket, &identity.node_name)?;
    let mut ready = BTreeSet::new();
    let mut expected = BTreeMap::new();
    let mut published = 0u64;
    let mut received = 0u64;
    let mut changed = 0u64;
    let mut value = 0x49;
    let mut held = None;
    let mut tick = tokio::time::interval(Duration::from_millis(34));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while received < 12 {
        consumer.check()?;
        tokio::select! {
            event = source.next_event() => {
                let event = event.context("source event stream closed")?;
                if let VideoSourceActorEvent::GenerationFailed { error, .. } = &event {
                    anyhow::bail!("source failed: {error}");
                }
                if let VideoSourceActorEvent::BufferReleased { sequence: 1, .. } = &event {
                    ensure!(received >= 6 && held.is_none(), "first buffer released before its sample is returned");
                }
                if let VideoSourceActorEvent::BufferAvailable { buffer_id, .. } = &event { ready.insert(*buffer_id); }
                output.handle_event(&event)?;
            }
            sample = consumer.next() => {
                let sample = sample?;
                let sequence = sample.buffer().context("sample buffer")?.offset();
                ensure!(sequence == received + 1, "missing or reordered frame {sequence}");
                let expected_value = expected.remove(&sequence).context("unpublished sample")?;
                pipewire_consumer::check_pixels(&sample, expected_value)?;
                if expected_value == 0x68 { changed += 1; }
                received += 1;
                if received == 1 {
                    held = Some(sample);
                    fixture.flip();
                    value = 0x68;
                }
                if let Some(first) = &held { pipewire_consumer::check_pixels(first, 0x49)?; }
                if received == 6 { drop(held.take()); }
            }
            _ = tick.tick(), if ready.len() == 3 && published < 12 => {
                match capture.capture().await {
                    Ok(frame) => {
                        let video = output.begin_publish(frame, published as i64 * 34_000_000, published == 0)?;
                        expected.insert(video.sequence, value);
                        source.publish(identity.media_generation, video).await?;
                        published += 1;
                    }
                    Err(CaptureError::Backpressure) => (),
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    ensure!(expected.is_empty(), "undelivered frames");
    ensure!(changed > 0, "no changed image reached the consumer");
    consumer.check()?;
    drop(consumer);
    let report = source.stop(identity.media_generation).await?;
    output.stopped(&report)?;
    source.shutdown().await?;
    drop(output);
    drop(capture.shutdown().await?);
    drop(control);
    ensure!(link.wait().await?.success(), "private port link failed");
    Ok(())
}
