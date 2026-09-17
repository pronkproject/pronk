//! Live delegated GPU rendering through the application capture port.
//! Requires a disposable compositor and isolated session bus; no DRM master fd.

mod decoder;
mod receiver_media;
mod renderer_consumer;

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk::display_state::{RouteTarget, RoutedMode};
use pronk::kernel_session::KernelSession;
use pronk::media_pipeline_port::{CaptureEventPort, CapturePipelinePort};
use pronk::media_session::{MediaRoute, MediaStartRequest, MediaStopReason};
use pronk::renderer_capture_pipeline::{
    RendererCapturePipeline, RendererCapturePipelineConfig, RendererPrivatePoolConfig,
};
use pronk_capture_broker::{Provider, Target};
use pronk_capture_receiver_test::Receiver;
use pronk_pipewire::{ClassifiedSocketPaths, ClassifiedSocketRemoteProvider};
use tokio_util::sync::CancellationToken;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

struct Probe {
    target: Target,
    width: u32,
    height: u32,
    refresh_millihz: u32,
    modifier: u64,
    socket: PathBuf,
    receiver: Option<SocketAddr>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize probe logging: {error}"))?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 8 || (args.len() == 10 && args[8] == "--receiver"),
        "expected device, CRTC, connector, width, height, refresh millihertz, output modifier and private socket, optionally --receiver IP:PORT; receiver mode interrupts playback"
    );
    let address: Option<SocketAddr> = args.get(9).map(|address| address.parse()).transpose()?;
    let device = std::fs::metadata(&args[0])?.rdev();
    let target = Target {
        device_major: nix::sys::stat::major(device).try_into()?,
        device_minor: nix::sys::stat::minor(device).try_into()?,
        crtc_id: NonZeroU32::new(args[1].parse()?).context("zero CRTC")?,
        connector_id: NonZeroU32::new(args[2].parse()?).context("zero connector")?,
    };
    let probe = Probe {
        target,
        width: args[3].parse()?,
        height: args[4].parse()?,
        refresh_millihz: args[5].parse()?,
        modifier: parse_u64(&args[6]).context("output modifier")?,
        socket: PathBuf::from(&args[7]),
        receiver: address,
    };
    let mut receiver = Receiver::default();
    let result = tokio::select! {
        result = tokio::time::timeout(
            Duration::from_secs(40),
            run(probe, &mut receiver),
        ) => result.context("renderer probe timed out").and_then(|result| result),
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => Err(anyhow::anyhow!("renderer probe interrupted")),
            Err(error) => Err(error.into()),
        },
    };
    let retired = receiver.shutdown().await;
    if let Err(error) = &retired {
        eprintln!("Receiver cleanup failed: {error:#}");
    }
    result?;
    retired?;
    println!(
        "PASS: live Mutter scene through delegated GPU rendering and private PipeWire DMA-BUFs"
    );
    if address.is_some() {
        println!("PASS: receiver acknowledged the delegated-renderer stream; visible playback still requires observation");
    }
    Ok(())
}

async fn run(probe: Probe, receiver: &mut Receiver) -> anyhow::Result<()> {
    let Probe {
        target,
        width,
        height,
        refresh_millihz,
        modifier,
        socket,
        receiver: address,
    } = probe;
    let mut pattern = tokio::process::Command::new(
        std::env::current_exe()?.with_file_name("pronk-capture-pattern-client"),
    )
    .kill_on_drop(true)
    .spawn()
    .context("start Wayland pattern")?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    ensure!(pattern.try_wait()?.is_none(), "Wayland pattern exited");

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
    let mut session: KernelSession = provider
        .acquire(target, CancellationToken::new())
        .await?
        .try_into()?;
    session.attach_monitor(None)?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let generation = nz64(u64::from(std::process::id()));
    let runtime = socket.parent().context("private socket has no directory")?;
    let remotes =
        ClassifiedSocketRemoteProvider::new(ClassifiedSocketPaths::in_runtime_dir(runtime)?);
    let capture_access = session.capture_access()?;
    let renderer_access = session.take_renderer_access()?;
    let (mut capture, mut renderer_events) = RendererCapturePipeline::new(
        renderer_access,
        capture_access,
        remotes,
        RendererCapturePipelineConfig {
            connector_id: target.connector_id,
            output_index: 0,
            session_id: format!("private-renderer-test-{generation}"),
            device_instance: "castkms-test".into(),
            node_description: "Live delegated renderer".into(),
            video_profile_id: "raw-dmabuf".into(),
            raw_layout: pronk_backend_protocol::RawVideoLayout::dma_buf(
                u32::from_le_bytes(*b"XR24"),
                modifier,
            ),
            video_bitrate: nz64(4_000_000),
            video_frame_rate: pronk_pipewire::VideoFrameRate::integer(nz(30)),
            private_pool: RendererPrivatePoolConfig {
                modifier,
                frame_capacity: NonZeroUsize::new(3).unwrap(),
                source_capacity: NonZeroUsize::new(3).unwrap(),
            },
            capture_pool_size: nz(4),
            capture_request_capacity: nz(3),
            capture_pool_byte_limit: nz64(128 * 1024 * 1024),
            capture_heap_path: "/dev/dma_heap/system".into(),
            capture_poll_interval: Duration::from_millis(2),
            capture_shutdown_timeout: Duration::from_secs(5),
        },
    )?;
    let mode = RoutedMode {
        width,
        height,
        refresh_millihz,
        flags: 0,
    };
    if address.is_some() {
        let media = receiver_media::run(
            &mut capture,
            MediaStartRequest {
                media_generation: generation.get(),
                route: MediaRoute {
                    route_generation: generation.get(),
                    target: RouteTarget::new(target.crtc_id),
                    mode,
                },
            },
            &socket,
            receiver,
            address,
        );
        tokio::pin!(media);
        tokio::select! {
            result = &mut media => result?,
            event = renderer_events.next_event() => {
                anyhow::bail!("renderer stopped while qualifying receiver output: {event:?}")
            }
        }
    } else {
        run_generation(
            &mut capture,
            &mut renderer_events,
            target,
            mode,
            generation,
            &socket,
        )
        .await?;
        let next_generation = nz64(
            generation
                .get()
                .checked_add(1)
                .context("generation overflow")?,
        );
        run_generation(
            &mut capture,
            &mut renderer_events,
            target,
            mode,
            next_generation,
            &socket,
        )
        .await?;
    }
    capture
        .shutdown(MediaStopReason::BackendShutdown, CancellationToken::new())
        .await?;
    session.release().await?;
    Ok(())
}

async fn run_generation(
    capture: &mut RendererCapturePipeline,
    renderer_events: &mut impl CaptureEventPort,
    target: Target,
    mode: RoutedMode,
    generation: NonZeroU64,
    socket: &Path,
) -> anyhow::Result<()> {
    let prepared = capture
        .start(
            MediaStartRequest {
                media_generation: generation.get(),
                route: MediaRoute {
                    route_generation: generation.get(),
                    target: RouteTarget::new(target.crtc_id),
                    mode,
                },
            },
            CancellationToken::new(),
        )
        .await?;
    let video = prepared.video_target;
    let mut link = tokio::process::Command::new("pw-link")
        .arg("--wait")
        .arg("--remote")
        .arg(socket)
        .arg(format!("{}:capture_1", video.node_name))
        .arg("pronk.renderer-test-consumer:input_1")
        .kill_on_drop(true)
        .spawn()
        .context("start private port link")?;
    let mut consumer = renderer_consumer::Consumer::start(socket, &video.node_name, &video.caps)?;
    capture
        .activate(generation, CancellationToken::new())
        .await?;

    let mut last_sequence = None;
    let mut held = None;
    for index in 0..12 {
        consumer.check()?;
        let buffer = tokio::select! {
            buffer = consumer.next() => buffer?,
            event = renderer_events.next_event() => {
                anyhow::bail!("renderer stopped while awaiting output: {event:?}");
            }
        };
        let sequence = renderer_consumer::check_buffer(&buffer)?;
        ensure!(
            last_sequence.is_none_or(|last| sequence > last),
            "renderer frame sequence did not advance"
        );
        last_sequence = Some(sequence);
        if index == 0 {
            held = Some(buffer);
        }
        if index == 5 {
            drop(held.take());
        }
    }
    drop(held);
    consumer.check()?;
    drop(consumer);
    capture
        .stop(
            generation,
            MediaStopReason::BackendShutdown,
            CancellationToken::new(),
        )
        .await?;
    ensure!(link.wait().await?.success(), "private port link failed");
    Ok(())
}

fn parse_u64(value: &str) -> anyhow::Result<u64> {
    match value.strip_prefix("0x") {
        Some(hex) => Ok(u64::from_str_radix(hex, 16)?),
        None => Ok(value.parse()?),
    }
}
