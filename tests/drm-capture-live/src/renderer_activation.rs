//! Requires an isolated test session bus with the live Mutter broker.
//! Does not open a DRM primary descriptor or submit a modeset directly.

use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use castkms_renderer::{
    CapabilityFormat, CapabilityProfile, FormatModifier, Profile, RendererCapability,
    StorageProvenance,
};
use drm_display_executor::scene::geometry::Extent;
use pronk_capture_broker::{Provider, RendererAccess, Target};
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
    println!("PASS: live Mutter broker renderer activation and HOST handback");
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
    let mut session = provider.acquire(target, CancellationToken::new()).await?;
    session.attach_monitor(None)?;
    let (capture, renderer_access) = wait_for_output(&mut session).await?;
    let (mut renderer, renderer_id, _, renderer_session) = renderer_access.into_parts()?;
    let before = renderer.describe()?;
    ensure!(before.profile() == Profile::HostV1);
    let candidate = renderer.begin_takeover(before)?;
    let output = candidate.configuration();
    let format = |fourcc, modifier| {
        CapabilityFormat::new(
            fourcc,
            modifier,
            NonZeroU32::new(1).unwrap(),
            StorageProvenance::new(true, true),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(u32::MAX).unwrap(),
        )
    };
    let capability = RendererCapability::single_primary_formats(
        Extent::new(output.width().get(), output.height().get())?,
        vec![
            format(u32::from_le_bytes(*b"XR24"), FormatModifier::Unspecified)?,
            format(u32::from_le_bytes(*b"XR24"), FormatModifier::Explicit(0))?,
            format(u32::from_le_bytes(*b"XB24"), FormatModifier::Unspecified)?,
            format(u32::from_le_bytes(*b"XB24"), FormatModifier::Explicit(0))?,
        ]
        .into_boxed_slice(),
    )?
    .with_output_color(256, true)?;
    let profile = CapabilityProfile::Renderer(capability);
    let registered = candidate
        .register_profile(&profile)
        .map_err(|failure| failure.into_parts().1)?;
    let transition = registered.registration().transition();
    let submitted = registered.submit_private_probe(None)?;
    renderer_session
        .install_transition(transition, CancellationToken::new())
        .await?;
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
    let host_access = renderer_session
        .acquire_renderer(CancellationToken::new())
        .await?;
    let (mut host_renderer, host_id, _, host_session) = host_access.into_parts()?;
    let gpu = host_renderer.describe()?;
    ensure!(gpu == after);
    let host = host_renderer
        .begin_takeover(gpu)?
        .register_profile(&CapabilityProfile::Host)
        .map_err(|failure| failure.into_parts().1)?;
    let host_transition = host.registration().transition();
    let host = host.into_host().map_err(|candidate| {
        let _ = candidate.abort();
        anyhow::anyhow!("registered HOST profile did not produce a HOST candidate")
    })?;
    host_session
        .install_transition(host_transition, CancellationToken::new())
        .await?;
    let returned = activate_host(host).await?;
    ensure!(returned.profile() == Profile::HostV1);
    ensure!(returned.generation().get() == after.generation().get() + 1);
    drop(host_renderer);
    host_session.release_renderer(host_id).await?;
    drop(active);
    drop(renderer);
    renderer_session.release_renderer(renderer_id).await?;
    capture.describe()?;
    drop(capture);
    session.release().await?;
    Ok(())
}

async fn activate_host(
    candidate: castkms_renderer::HostCandidate<'_, OwnedFd>,
) -> anyhow::Result<castkms_renderer::Description> {
    let mut pending = match candidate.activate() {
        Ok(active) => {
            let description = active.description();
            drop(active);
            return Ok(description);
        }
        Err(error) if error.error().raw_os_error() == Some(nix::libc::EAGAIN) => {
            error.into_candidate()
        }
        Err(error) => return Err(error.into_error().into()),
    };
    loop {
        tokio::time::sleep(Duration::from_millis(2)).await;
        match pending.activate() {
            Ok(active) => {
                let description = active.description();
                drop(active);
                return Ok(description);
            }
            Err(error) if error.error().raw_os_error() == Some(nix::libc::EAGAIN) => {
                pending = error.into_candidate();
            }
            Err(error) => return Err(error.into_error().into()),
        }
    }
}

async fn wait_for_output(
    session: &mut pronk_capture_broker::Session,
) -> anyhow::Result<(drm_capture::Client, RendererAccess)> {
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
        let renderer = match session.renderer_access().and_then(|access| {
            access.open()?;
            Ok(access)
        }) {
            Ok(renderer) => renderer,
            Err(error) if transient(&error) && Instant::now() < deadline => {
                drop(capture);
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        drop(renderer);
        return Ok((capture, session.take_renderer_access()?));
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
