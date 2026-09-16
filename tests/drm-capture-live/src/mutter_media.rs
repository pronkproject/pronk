//! Live Mutter -> broker -> continuous capture -> PipeWire -> H.264 -> decoder.
//! Requires a disposable compositor and isolated session bus; no DRM master fd.

mod decoder;
use pronk_capture_receiver_test as receiver;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::unix::{fs::MetadataExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk::display_state::{RouteTarget, RoutedMode};
use pronk::drm_capture_pipeline::{DrmCapturePipeline, DrmCapturePipelineConfig};
use pronk::media_pipeline_port::CapturePipelinePort;
use pronk::media_session::{MediaRoute, MediaStartRequest, MediaStopReason};
use pronk_capture_broker::{Provider, Target};
use pronk_media::{
    MediaGraphActor, MediaGraphConfiguration, PipeWireVideoInput, VideoCadence, VideoCodec,
    VideoFrameDependency,
};
use pronk_pipewire::{ClassifiedSocketPaths, ClassifiedSocketRemoteProvider};
use tokio_util::sync::CancellationToken;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}
fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
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
        args.len() == 6 || (args.len() == 8 && args[6] == "--receiver"),
        "expected device, CRTC, connector, width, height, private socket, optionally --receiver IP:PORT; receiver mode interrupts playback"
    );
    let address: Option<SocketAddr> = args.get(7).map(|address| address.parse()).transpose()?;
    let device = std::fs::metadata(&args[0])?.rdev();
    let target = Target {
        device_major: nix::sys::stat::major(device).try_into()?,
        device_minor: nix::sys::stat::minor(device).try_into()?,
        crtc_id: NonZeroU32::new(args[1].parse()?).context("zero CRTC")?,
        connector_id: NonZeroU32::new(args[2].parse()?).context("zero connector")?,
    };
    let width: u32 = args[3].parse()?;
    let height: u32 = args[4].parse()?;
    let socket = PathBuf::from(&args[5]);
    let mut receiver = receiver::Receiver::default();
    let result = tokio::select! {
        result = tokio::time::timeout(
            Duration::from_secs(40),
            run(target, width, height, &socket, &mut receiver, address),
        ) => result.context("capture probe timed out").and_then(|result| result),
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => Err(anyhow::anyhow!("capture probe interrupted")),
            Err(error) => Err(error.into()),
        },
    };
    let retired = receiver.shutdown().await;
    if let Err(error) = &retired {
        eprintln!("Receiver cleanup failed: {error:#}");
    }
    result?;
    retired?;
    println!("PASS: live Mutter scene through broker, continuous capture, PipeWire, production H.264 and decoded changing pixels");
    if address.is_some() {
        println!("PASS: receiver acknowledged the captured stream; visible playback still requires observation");
    }
    Ok(())
}

async fn run(
    target: Target,
    width: u32,
    height: u32,
    socket: &Path,
    receiver: &mut receiver::Receiver,
    address: Option<SocketAddr>,
) -> anyhow::Result<()> {
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
    let session = provider.acquire(target, CancellationToken::new()).await?;
    session.attach_monitor(None)?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let generation = nz64(u64::from(std::process::id()));
    let runtime = socket.parent().context("private socket has no directory")?;
    let remotes =
        ClassifiedSocketRemoteProvider::new(ClassifiedSocketPaths::in_runtime_dir(runtime)?);
    let (mut capture, _capture_events) = DrmCapturePipeline::new(
        session.capture_access()?,
        remotes,
        DrmCapturePipelineConfig {
            connector_id: target.connector_id,
            output_index: 0,
            session_id: format!("private-test-{generation}"),
            device_instance: "castkms-test".into(),
            node_description: "Live Mutter capture".into(),
            video_profile_id: "h264".into(),
            video_bitrate: nz64(4_000_000),
            capture_rate_hz: nz(30),
            pool_size: nz(4),
            request_capacity: nz(3),
            pool_byte_limit: nz64(128 * 1024 * 1024),
            heap_path: "/dev/dma_heap/system".into(),
            poll_interval: Duration::from_millis(2),
            shutdown_timeout: Duration::from_secs(5),
        },
    );
    let request = MediaStartRequest {
        media_generation: generation.get(),
        route: MediaRoute {
            route_generation: 1,
            target: RouteTarget::new(target.crtc_id),
            mode: RoutedMode {
                width,
                height,
                refresh_millihz: 60_000,
                flags: 0,
            },
        },
    };
    let prepared = capture.start(request, CancellationToken::new()).await?;
    if let Some(address) = address {
        eprintln!(
            "Starting an explicit receiver test at {address}; current playback will be interrupted"
        );
        receiver.start(address, width, height).await?;
    }
    let video_target = prepared.video_target;
    let (media, mut encoded) = MediaGraphActor::spawn_with_output(16)?;
    let config = MediaGraphConfiguration {
        media_generation: generation,
        video: PipeWireVideoInput {
            remote: UnixStream::connect(socket)?.into(),
            node_name: video_target.node_name.clone(),
            object_serial: video_target.object_serial,
            caps: video_target.caps,
        },
        audio: None,
        video_codec: VideoCodec::H264,
        video_cadence: VideoCadence::new(nz(30), nz(1)),
        video_bitrate: nz64(4_000_000),
    };
    let mut decoder = decoder::Decoder::new()?;
    let mut received = 0;
    let mut decoded = 0;
    let mut colors = BTreeSet::new();
    let mut last_timestamp = None;
    let mut acknowledged = 0;
    let receiver_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    {
        let mut started = false;
        let activation = async {
            media.configure(config).await?;
            capture
                .activate(generation, CancellationToken::new())
                .await?;
            media.start(generation).await.map_err(anyhow::Error::from)
        };
        tokio::pin!(activation);
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        while decoded < 12
            || colors.len() < 2
            || !started
            || (address.is_some()
                && (acknowledged < 30 || tokio::time::Instant::now() < receiver_deadline))
        {
            tokio::select! {
                result = &mut activation, if !started => { result?; started = true; }
                frame = encoded.recv() => {
                    let frame = frame.context("encoded output stopped")?;
                    if received == 0 { ensure!(frame.dependency == VideoFrameDependency::KeyFrame, "initial key frame missing"); }
                    ensure!(frame.media_generation == generation && last_timestamp.is_none_or(|last| frame.media_timestamp > last), "encoded identity/timing");
                    ensure!(!frame.data.is_empty() && !frame.duration.is_zero(), "empty encoded frame or duration");
                    last_timestamp = Some(frame.media_timestamp);
                    if address.is_some() { receiver.send(frame.clone()).await?; }
                    decoder.push(frame)?;
                    received += 1;
                }
                pixels = decoder.next(width, height) => { colors.insert(pixels?); decoded += 1; }
                event = receiver.next_event(), if address.is_some() => {
                    match event? {
                        receiver::SenderEvent::NeedsKeyFrame { .. } => media.request_key_frame(generation).await?,
                        receiver::SenderEvent::ReceiverTimedOut => anyhow::bail!("receiver acknowledgements timed out"),
                        receiver::SenderEvent::FatalError(error) => return Err(error.into()),
                        _ => (),
                    }
                }
                _ = tick.tick() => {
                    decoder.check()?;
                    let snapshot = media.snapshot();
                    ensure!(snapshot.state != pronk_media::MediaGraphState::Failed, "media failed: {:?}", snapshot.last_error);
                    eprintln!("Mutter capture {width}x{height} encoded={received} decoded={decoded} colors={colors:?}");
                    if address.is_some() {
                        let statistics = receiver.statistics().await?;
                        acknowledged = statistics.frames_acked;
                        eprintln!("receiver acknowledged={acknowledged} in_flight={}", statistics.in_flight_frames);
                        if acknowledged == 0 { media.request_key_frame(generation).await?; }
                    }
                }
            }
        }
    }
    media.stop(generation).await?;
    media.shutdown().await?;
    drop(decoder);
    capture
        .stop(
            generation,
            MediaStopReason::BackendShutdown,
            CancellationToken::new(),
        )
        .await?;
    capture
        .shutdown(MediaStopReason::BackendShutdown, CancellationToken::new())
        .await?;
    session.release().await?;
    pattern.kill().await?;
    eprintln!("Mutter encoded={received} decoded={decoded} colors={colors:?}");
    Ok(())
}
