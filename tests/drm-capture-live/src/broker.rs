//! Requires an isolated test session bus with the live Mutter broker.
//! Does not open a DRM primary descriptor or submit a modeset directly.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use castkms_renderer::{CapabilityProfile, RendererCapability};
use drm_display_executor::scene::geometry::Extent;
use pronk_capture::{allocation::Heap, Config, Layout, Session};
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
    println!("PASS: live Mutter broker capture, renderer startup, release and reacquisition");
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
        session.attach_monitor(None)?;
        let (client, mut renderer) = wait_for_output(&session).await?;
        let witness = client.as_fd().try_clone_to_owned()?;
        let renderer_witness = renderer.as_fd().try_clone_to_owned()?;
        let offer = client.describe()?;
        let buffers = heap.allocate(
            Layout {
                width: offer.width,
                height: offer.height,
            },
            nz(3),
            NonZeroU64::new(64 * 1024 * 1024).unwrap(),
        )?;
        let actor = Session::new(client).spawn(
            buffers,
            Config {
                capacity: nz(3),
                poll_interval: Duration::from_millis(2),
                shutdown_timeout: Duration::from_secs(5),
            },
        )?;
        let frame = actor.capture().await?;
        ensure!(!frame.timestamp().is_zero(), "missing capture timestamp");
        let description = renderer.describe()?;
        let candidate = renderer.begin_takeover(description)?;
        let configuration = candidate.configuration();
        ensure!(
            configuration.width() == offer.width && configuration.height() == offer.height,
            "renderer and capture output geometry differs"
        );
        let profile = CapabilityProfile::Renderer(RendererCapability::linear_xrgb8888_primary(
            Extent::new(configuration.width().get(), configuration.height().get())?,
        ));
        let startup = candidate
            .register_profile(&profile)
            .map_err(|failure| failure.into_parts().1)?
            .startup_image()?;
        let image = startup.image();
        ensure!(
            image.width() == offer.width && image.height() == offer.height,
            "startup image geometry differs"
        );
        ensure!(
            image.pitch().get()
                >= offer
                    .width
                    .get()
                    .checked_mul(4)
                    .context("capture width exceeds the startup image pitch domain")?,
            "startup image pitch is too small"
        );
        ensure!(
            image.content_serial().is_some(),
            "captured HOST content has no identity"
        );
        let startup_serial = image.content_serial().unwrap();
        startup.submit_probe(None)?.abort()?;
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
            "broker pass={pass} capture={}x{} request={} startup_serial={}",
            frame.layout().width,
            frame.layout().height,
            frame.request().get(),
            startup_serial
        );
        drop(renderer);
        drop(actor.shutdown().await?);
        session.release().await?;
        let revoked = drm_capture::Client::from_fd(witness)
            .err()
            .context("released broker grant remained active")?;
        ensure!(
            revoked.raw_os_error() == Some(nix::libc::EKEYREVOKED),
            "unexpected released-grant error: {revoked}"
        );
        let revoked = castkms_renderer::Renderer::from_fd(renderer_witness)
            .err()
            .context("released renderer capability remained active")?;
        ensure!(
            revoked.raw_os_error() == Some(nix::libc::EKEYREVOKED),
            "unexpected released-renderer error: {revoked}"
        );
        held = Some(frame);
    }
    drop(held);
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
