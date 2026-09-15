//! Requires an isolated test session bus with the live Mutter broker.
//! Does not open a DRM primary descriptor or submit a modeset directly.

use std::num::{NonZeroU32, NonZeroUsize};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use castkms_renderer::Profile;
use pronk_capture_broker::{Provider, Target};
use tokio_util::sync::CancellationToken;

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
    println!("PASS: live Mutter broker renderer activation and HOST cutoff");
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
    let session = provider.acquire(target, CancellationToken::new()).await?;
    session.attach_monitor(None)?;
    let (capture, mut renderer) = wait_for_output(&session).await?;
    let before = renderer.describe()?;
    ensure!(before.profile() == Profile::HostV1);
    let submitted = renderer
        .begin_takeover(before)?
        .submit_private_probe(None)?;
    let active = submitted.activate().map_err(|error| error.into_error())?;
    let after = active.description();
    ensure!(after.profile() == Profile::GpuV1);
    ensure!(after.generation().get() == before.generation().get() + 1);
    ensure!(renderer_description(&active)? == after);
    let error = capture
        .describe()
        .err()
        .context("capture description remained available after activation")?;
    ensure!(error.raw_os_error() == Some(nix::libc::EOPNOTSUPP));
    drop(active);
    drop(renderer);
    drop(capture);
    session.release().await?;
    Ok(())
}

async fn wait_for_output(
    session: &pronk_capture_broker::Session,
) -> anyhow::Result<(drm_capture::Client, castkms_renderer::Renderer)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let capture = match session.open_capture() {
            Ok(capture) => capture,
            Err(error) if transient(&error) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let renderer = match session.open_renderer() {
            Ok(renderer) => renderer,
            Err(error) if transient(&error) && Instant::now() < deadline => {
                drop(capture);
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        return Ok((capture, renderer));
    }
}

fn transient(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(nix::libc::EACCES | nix::libc::EAGAIN | nix::libc::ENODEV | nix::libc::ESTALE)
    )
}

fn renderer_description(
    renderer: &impl std::os::fd::AsFd,
) -> anyhow::Result<castkms_renderer::Description> {
    let fd = renderer.as_fd().try_clone_to_owned()?;
    Ok(castkms_renderer::Renderer::from_fd(fd)?.describe()?)
}
