//! Revocation before a PipeWire consumer attaches, using a disposable display.

#[allow(dead_code)]
mod fixture;

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let device = PathBuf::from(args.next().context("expected unused VM DRM device")?);
    let _socket = args.next().context("expected private PipeWire socket")?;
    ensure!(args.next().is_none(), "expected device and socket only");
    tokio::time::timeout(Duration::from_secs(15), run(&device)).await??;
    println!("PASS: idle grant revocation stops video without an attached consumer");
    Ok(())
}

async fn run(device: &Path) -> anyhow::Result<()> {
    let fixture = fixture::Fixture::open(device)?;
    let (client, control) = drm_capture::create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let actor = Session::new(client).spawn(
        Heap::open(Path::new("/dev/dma_heap/system"))?.allocate(
            Layout {
                width: nz(640),
                height: nz(480),
            },
            nz(3),
            NonZeroU64::new(16 * 1024 * 1024).unwrap(),
        )?,
        Config {
            capacity: nz(3),
            poll_interval: Duration::from_millis(2),
            shutdown_timeout: Duration::from_secs(5),
        },
    )?;
    let video = Video::start(
        actor,
        VideoSourceConfig {
            node_name: "pronk.idle-revoke-test".into(),
            node_description: "Idle capture revocation".into(),
            session_id: "private-test".into(),
            device_instance: "castkms-test".into(),
            connector_id: nz(fixture.connector()),
            output_index: 0,
            media_generation: NonZeroU64::new(1).unwrap(),
            frame_rate: pronk_pipewire::VideoFrameRate::integer(nz(30)),
        },
        PipeWireRemote::AmbientDevelopment,
    )
    .await?;
    let mut state = video.subscribe();
    ensure!(*state.borrow() == State::Active, "initial video state");
    drop(control);
    state
        .wait_for(|state| matches!(state, State::Failed(_)))
        .await?;
    ensure!(
        video.shutdown().await.is_err(),
        "revocation reported successful shutdown"
    );
    Ok(())
}
