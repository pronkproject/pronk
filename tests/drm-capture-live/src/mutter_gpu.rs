//! Mutter scene through the delegated renderer into GPU-owned recipient images.

mod monitor;

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_capture::{Buffer, Config, Session};
use pronk_capture_broker::{Provider, Target};
use pronk_gpu::vulkan::{test_support::readback, Device, PackedFormat};
use tokio_util::sync::CancellationToken;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 5,
        "expected device, CRTC, connector, width, height"
    );
    let device = std::fs::metadata(&args[0])?.rdev();
    let target = Target {
        device_major: nix::sys::stat::major(device).try_into()?,
        device_minor: nix::sys::stat::minor(device).try_into()?,
        crtc_id: nz(args[1].parse()?),
        connector_id: nz(args[2].parse()?),
    };
    let width: u32 = args[3].parse()?;
    let height: u32 = args[4].parse()?;
    tokio::time::timeout(Duration::from_secs(45), run(target, width, height))
        .await
        .context("GPU capture probe timed out")??;
    println!("PASS: changing nonblack Mutter pixels reached GPU-owned destinations");
    Ok(())
}

async fn run(target: Target, width: u32, height: u32) -> anyhow::Result<()> {
    let connection = zbus::Connection::session().await?;
    let acquired = connection
        .request_name_with_flags(
            "io.github.pronkproject.Pronk1",
            zbus::fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await?;
    ensure!(
        acquired == zbus::fdo::RequestNameReply::PrimaryOwner,
        "Pronk name is already owned"
    );
    let provider = Provider::new(
        connection,
        NonZeroUsize::new(1).unwrap(),
        Duration::from_secs(5),
    )?;
    let display = provider.acquire(target, CancellationToken::new()).await?;
    display.attach_monitor(Some(&monitor::edid(width, height, 60_000)?))?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pattern_binary = std::env::var_os("PRONK_PATTERN_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_exe()?.with_file_name("pronk-capture-pattern-client"));
    let mut pattern = tokio::process::Command::new(pattern_binary)
        .arg(width.to_string())
        .arg(height.to_string())
        .kill_on_drop(true)
        .spawn()
        .context("start Wayland pattern")?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    ensure!(pattern.try_wait()?.is_none(), "Wayland pattern exited");

    let client = display.open_capture()?;
    let description = client.describe()?;
    ensure!(
        description.width.get() == width && description.height.get() == height,
        "capture dimensions differ from the active mode"
    );
    let gpu = Device::open("/dev/dri/renderD128")?;
    let format = match description.format {
        value if value == u32::from_le_bytes(*b"XR24") || value == u32::from_le_bytes(*b"AR24") => {
            PackedFormat::Bgra8
        }
        value if value == u32::from_le_bytes(*b"XB24") || value == u32::from_le_bytes(*b"AB24") => {
            PackedFormat::Rgba8
        }
        _ => anyhow::bail!("unsupported capture format"),
    };
    let image = gpu.allocate_with_format(
        format,
        description.width,
        description.height,
        description.modifier,
    )?;
    let (image, initial_clear) = image.clear_and_wait([240, 0, 240])?;
    drop(initial_clear);
    let (mut image, initial_pixels) = readback(image);
    let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
    let initial = initial_pixels
        .get(center..center + 3)
        .context("initial image is smaller than its advertised layout")?;
    ensure!(
        initial[0] > 200 && initial[1] < 20 && initial[2] > 200,
        "Vulkan readback did not observe the initial magenta clear: {initial:?}"
    );
    let layout = image.layout();
    let buffer = Buffer::new_drm(
        image.export()?,
        description.format,
        layout.modifier,
        NonZeroU32::new(layout.pitch.try_into()?).context("zero GPU pitch")?,
        layout.offset.try_into()?,
        NonZeroU64::new(layout.allocation_size).context("zero GPU allocation")?,
    )?;
    let mut capture = Session::new(client);
    let actor = capture.spawn_for_offer(
        vec![buffer],
        Config {
            capacity: nz(1),
            poll_interval: Duration::from_millis(2),
            shutdown_timeout: Duration::from_secs(5),
        },
        description,
    )?;
    let mut first_time = None;
    let mut samples = Vec::new();
    for index in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(12), actor.capture())
            .await
            .context("GPU capture request timed out")?
            .context("GPU capture request failed")?;
        if let Some(previous) = first_time {
            ensure!(
                frame.timestamp() > previous,
                "capture timestamp did not advance"
            );
        }
        first_time = Some(frame.timestamp());
        let (returned, pixels) = readback(image);
        image = returned;
        let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
        let sample = pixels
            .get(center..center + 3)
            .context("captured image is smaller than its advertised layout")?;
        let sample: [u8; 3] = sample.try_into()?;
        samples.push(sample);
        eprintln!(
            "GPU capture frame {} completed with pixel {sample:?}",
            index + 1
        );
        drop(frame);
        if index < 2 {
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    }
    ensure!(
        samples
            .iter()
            .any(|sample| sample.iter().all(|channel| *channel > 24)),
        "all captured center pixels are black or incomplete: {samples:?}"
    );
    ensure!(
        samples.windows(2).any(|pair| pair[0] != pair[1]),
        "captured pixel shade did not change: {samples:?}"
    );
    drop(actor.shutdown().await?);
    display.release().await?;
    pattern.kill().await?;
    Ok(())
}
