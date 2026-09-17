//! Continuous capture owner with a real PipeWire consumer in a disposable VM.

#[allow(dead_code)]
mod fixture;
mod pipewire_consumer;

use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_capture::{allocation::Heap, Config, Layout, Session};
use pronk_capture_pipewire::{State, Video};
use pronk_pipewire::{PipeWireRemote, VideoSourceConfig};

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
        "PASS: continuous capture video owner, changing pixels, held sample and joined shutdown"
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
    let actor = Session::new(client).spawn(
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
    let video = Video::prepare(
        actor,
        VideoSourceConfig {
            node_name: "pronk.video-owner-test".into(),
            node_description: "Capture video owner test".into(),
            session_id: "private-test".into(),
            device_instance: "castkms-test".into(),
            connector_id: nz(fixture.connector()),
            output_index: 0,
            media_generation: nz64(1),
            frame_rate: pronk_pipewire::VideoFrameRate::integer(nz(30)),
        },
        PipeWireRemote::AmbientDevelopment,
    )
    .await?;
    let mut state = video.subscribe();
    let mut link = tokio::process::Command::new("pw-link")
        .arg("--wait")
        .arg("--remote")
        .arg(socket)
        .arg(format!("{}:capture_1", video.identity().node_name))
        .arg("pronk.capture-test-consumer:input_1")
        .kill_on_drop(true)
        .spawn()
        .context("start video port link")?;
    let mut consumer =
        pipewire_consumer::Consumer::start(socket, &video.identity().node_name, false)?;
    ensure!(link.wait().await?.success(), "video port link failed");
    ensure!(
        tokio::time::timeout(Duration::from_millis(250), consumer.next())
            .await
            .is_err(),
        "prepared capture produced a frame before activation"
    );
    video.activate().await?;
    let mut held = None;
    let mut changed = 0;
    let mut received = 0;
    while received < 12 || changed == 0 {
        consumer.check()?;
        tokio::select! {
            sample = consumer.next() => {
                let sample = sample?;
                let value = *sample.buffer().context("sample")?.map_readable()?.first().context("empty sample")?;
                ensure!(value == 0x49 || value == 0x68, "unexpected captured image");
                let sequence = pipewire_consumer::check_pixels(&sample, value)?;
                ensure!(sequence == received + 1, "missing or reordered frame");
                received += 1;
                if changed > 0 { ensure!(value == 0x68, "old image followed new image"); }
                if value == 0x68 { changed += 1; }
                if received == 1 {
                    ensure!(value == 0x49, "incorrect first image");
                    held = Some(sample);
                    fixture.flip();
                }
                if let Some(first) = &held { pipewire_consumer::check_pixels(first, 0x49)?; }
                if received == 6 { drop(held.take()); }
            }
            result = state.changed() => {
                result?;
                ensure!(*state.borrow() == State::Active, "video stopped: {:?}", *state.borrow());
            }
        }
    }
    drop(held);
    drop(consumer);
    drop(video.shutdown().await?);
    ensure!(*state.borrow() == State::Stopped, "shutdown state");
    drop(control);
    Ok(())
}
