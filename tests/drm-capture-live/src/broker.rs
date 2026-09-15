//! Requires an isolated test session bus with the live Mutter broker.
//! Does not open a DRM primary descriptor or change the displayed configuration.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_capture::{allocation::Heap, Actor, Config, Layout};
use pronk_capture_broker::{Provider, Target};
use tokio_util::sync::CancellationToken;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 3,
        "expected DRM device path, CRTC ID and connector ID on an isolated test bus"
    );
    let device = std::fs::metadata(&args[0])?.rdev();
    let target = Target {
        device_major: nix::sys::stat::major(device).try_into()?,
        device_minor: nix::sys::stat::minor(device).try_into()?,
        crtc_id: NonZeroU32::new(args[1].parse()?).context("zero CRTC")?,
        connector_id: NonZeroU32::new(args[2].parse()?).context("zero connector")?,
    };
    tokio::time::timeout(Duration::from_secs(30), run(target)).await??;
    println!(
        "PASS: live Mutter broker acquisition, actor capture, explicit release and reacquisition"
    );
    Ok(())
}

async fn run(target: Target) -> anyhow::Result<()> {
    let connection = zbus::Connection::session().await?;
    let acquired = connection
        .request_name_with_flags(
            "io.github.pronkproject.Pronk1",
            zbus::fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await?;
    ensure!(
        acquired == zbus::fdo::RequestNameReply::PrimaryOwner,
        "test Pronk name is already owned"
    );
    let provider = Provider::new(
        connection,
        NonZeroUsize::new(1).unwrap(),
        Duration::from_secs(5),
    )?;
    let heap = Heap::open(Path::new("/dev/dma_heap/system"))?;
    let mut held = None;
    for pass in 0..2 {
        let session = provider.acquire(target, CancellationToken::new()).await?;
        let client = session.open_capture()?;
        let witness = client.as_fd().try_clone_to_owned()?;
        let offer = client.describe()?;
        let buffers = heap.allocate(
            Layout {
                width: offer.width,
                height: offer.height,
            },
            nz(3),
            NonZeroU64::new(64 * 1024 * 1024).unwrap(),
        )?;
        let actor = Actor::spawn(
            client,
            buffers,
            Config {
                capacity: nz(3),
                poll_interval: Duration::from_millis(2),
                shutdown_timeout: Duration::from_secs(5),
            },
        )?;
        let frame = actor.capture().await?;
        ensure!(!frame.timestamp().is_zero(), "missing capture timestamp");
        if let Some(old) = &held {
            ensure!(
                actor
                    .buffers()
                    .iter()
                    .all(|buffer| !buffer.contains_frame(old)),
                "old session storage was reused"
            );
        }
        eprintln!(
            "broker pass={pass} capture={}x{} request={}",
            frame.layout().width,
            frame.layout().height,
            frame.request().get()
        );
        drop(actor.shutdown().await?);
        session.release().await?;
        let revoked = drm_capture::Client::from_fd(witness)
            .err()
            .context("released broker grant remained active")?;
        ensure!(
            revoked.raw_os_error() == Some(nix::libc::EKEYREVOKED),
            "unexpected released-grant error: {revoked}"
        );
        held = Some(frame);
    }
    drop(held);
    Ok(())
}
