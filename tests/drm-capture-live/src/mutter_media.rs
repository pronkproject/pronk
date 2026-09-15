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
use pronk_capture::{allocation::Heap, Actor, Config, Layout};
use pronk_capture_broker::{Provider, Target};
use pronk_capture_pipewire::{State, Video};
use pronk_media::{
    MediaGraphActor, MediaGraphConfiguration, PipeWireVideoInput, VideoCodec, VideoFrameDependency,
};
use pronk_pipewire::{PipeWireRemote, VideoSourceConfig};
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
        args.len() == 4 || (args.len() == 6 && args[4] == "--receiver"),
        "expected device, CRTC, connector, private socket, optionally --receiver IP:PORT; receiver mode interrupts playback"
    );
    let address: Option<SocketAddr> = args.get(5).map(|address| address.parse()).transpose()?;
    let device = std::fs::metadata(&args[0])?.rdev();
    let target = Target {
        device_major: nix::sys::stat::major(device).try_into()?,
        device_minor: nix::sys::stat::minor(device).try_into()?,
        crtc_id: NonZeroU32::new(args[1].parse()?).context("zero CRTC")?,
        connector_id: NonZeroU32::new(args[2].parse()?).context("zero connector")?,
    };
    let socket = PathBuf::from(&args[3]);
    let mut receiver = receiver::Receiver::default();
    let result = tokio::select! {
        result = tokio::time::timeout(
            Duration::from_secs(40),
            run(target, &socket, &mut receiver, address),
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
    let client = provider
        .acquire(target, CancellationToken::new())
        .await?
        .into_capture()?;
    let offer = client.describe()?;
    if let Some(address) = address {
        eprintln!(
            "Starting an explicit receiver test at {address}; current playback will be interrupted"
        );
        receiver
            .start(address, offer.width.get(), offer.height.get())
            .await?;
    }
    let buffers = Heap::open(Path::new("/dev/dma_heap/system"))?.allocate(
        Layout {
            width: offer.width,
            height: offer.height,
        },
        nz(3),
        nz64(128 * 1024 * 1024),
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
    let generation = nz64(u64::from(std::process::id()));
    let video = Video::start(
        actor,
        VideoSourceConfig {
            node_name: format!("pronk.mutter-media-test-{generation}"),
            node_description: "Live Mutter capture".into(),
            session_id: format!("private-test-{generation}"),
            device_instance: "castkms-test".into(),
            connector_id: target.connector_id,
            output_index: 0,
            media_generation: generation,
            refresh_hz: nz(30),
        },
        PipeWireRemote::AmbientDevelopment,
    )
    .await?;
    let identity = video.identity().clone();
    let mut state = video.subscribe();
    let (media, mut encoded) = MediaGraphActor::spawn_with_output(16)?;
    let config = MediaGraphConfiguration {
        media_generation: identity.media_generation,
        video: PipeWireVideoInput {
            remote: UnixStream::connect(socket)?.into(),
            node_name: identity.node_name.clone(),
            object_serial: identity.object_serial,
            caps: format!(
                "video/x-raw,format=BGRx,width={},height={},framerate=30/1",
                offer.width, offer.height
            ),
        },
        audio: None,
        video_codec: VideoCodec::H264,
        video_bitrate: nz64(4_000_000),
    };
    let mut decoder = decoder::Decoder::new()?;
    let mut link = tokio::process::Command::new("pw-link")
        .arg("--wait")
        .arg("--remote")
        .arg(socket)
        .arg(format!("{}:capture_1", identity.node_name))
        .arg(format!("pronk-backend-media-{}:input_1", identity.media_generation))
        .kill_on_drop(true)
        .spawn()
        .context("link media input")?;
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
            media.start(identity.media_generation).await
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
                result = state.changed() => {
                    result?;
                    ensure!(*state.borrow() == State::Active, "capture failed: {:?}", *state.borrow());
                }
                frame = encoded.recv() => {
                    let frame = frame.context("encoded output stopped")?;
                    if received == 0 { ensure!(frame.dependency == VideoFrameDependency::KeyFrame, "initial key frame missing"); }
                    ensure!(frame.media_generation == identity.media_generation && last_timestamp.is_none_or(|last| frame.media_timestamp > last), "encoded identity/timing");
                    ensure!(!frame.data.is_empty() && !frame.duration.is_zero(), "empty encoded frame or duration");
                    last_timestamp = Some(frame.media_timestamp);
                    if address.is_some() { receiver.send(frame.clone()).await?; }
                    decoder.push(frame)?;
                    received += 1;
                }
                pixels = decoder.next(offer.width.get(), offer.height.get()) => { colors.insert(pixels?); decoded += 1; }
                event = receiver.next_event(), if address.is_some() => {
                    match event? {
                        receiver::SenderEvent::NeedsKeyFrame { .. } => media.request_key_frame(identity.media_generation).await?,
                        receiver::SenderEvent::ReceiverTimedOut => anyhow::bail!("receiver acknowledgements timed out"),
                        receiver::SenderEvent::FatalError(error) => return Err(error.into()),
                        _ => (),
                    }
                }
                _ = tick.tick() => {
                    decoder.check()?;
                    let snapshot = media.snapshot();
                    ensure!(snapshot.state != pronk_media::MediaGraphState::Failed, "media failed: {:?}", snapshot.last_error);
                    eprintln!("Mutter capture {}x{} encoded={received} decoded={decoded} colors={colors:?}", offer.width, offer.height);
                    if address.is_some() {
                        let statistics = receiver.statistics().await?;
                        acknowledged = statistics.frames_acked;
                        eprintln!("receiver acknowledged={acknowledged} in_flight={}", statistics.in_flight_frames);
                        if acknowledged == 0 { media.request_key_frame(identity.media_generation).await?; }
                    }
                }
            }
        }
    }
    media.stop(identity.media_generation).await?;
    media.shutdown().await?;
    drop(decoder);
    video.shutdown().await?.release().await?;
    ensure!(*state.borrow() == State::Stopped, "video shutdown state");
    pattern.kill().await?;
    ensure!(link.wait().await?.success(), "media link failed");
    eprintln!("Mutter encoded={received} decoded={decoded} colors={colors:?}");
    Ok(())
}
