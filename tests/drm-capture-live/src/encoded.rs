//! Reference capture through the production H.264 media actor and a decoder.

mod decoder;
#[allow(dead_code)]
mod fixture;

use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_capture::{allocation::Heap, CaptureError, Config, Layout, Session};
use pronk_capture_pipewire::Registration;
use pronk_media::{
    MediaGraphActor, MediaGraphConfiguration, PipeWireVideoInput, VideoCadence, VideoCodec,
    VideoEncoder, VideoFrameDependency,
};
use pronk_pipewire::{
    PipeWireRemote, VideoSourceActor, VideoSourceActorEvent, VideoSourceConfig,
    VideoSourceGeneration,
};

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
        "PASS: changing kernel capture through production H.264 media actor and decoded pixels"
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
    let capture = Session::new(client).spawn(
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
    let registration = Registration::new(&capture)?;
    let mut source = VideoSourceActor::spawn()?;
    let identity = source
        .start(VideoSourceGeneration {
            config: VideoSourceConfig {
                node_name: "pronk.encoder-test".into(),
                node_description: "Kernel capture encoding test".into(),
                session_id: "private-test".into(),
                device_instance: "castkms-test".into(),
                connector_id: nz(fixture.connector()),
                output_index: 0,
                media_generation: nz64(1),
                frame_rate: pronk_pipewire::VideoFrameRate::integer(nz(30)),
            },
            buffers: registration.export()?,
            remote: PipeWireRemote::AmbientDevelopment,
        })
        .await?;
    let mut output = registration.bind(identity.clone());
    let (media, mut encoded) = MediaGraphActor::spawn_with_output(16)?;
    let configuration = MediaGraphConfiguration {
        media_generation: identity.media_generation,
        video: PipeWireVideoInput {
            remote: UnixStream::connect(socket)?.into(),
            node_name: identity.node_name.clone(),
            object_serial: identity.object_serial,
            caps: "video/x-raw,format=BGRx,width=640,height=480,framerate=30/1".into(),
        },
        audio: None,
        video_encoder: VideoEncoder::software(VideoCodec::H264),
        video_cadence: VideoCadence::new(nz(30), nz(1)),
        video_bitrate: nz64(2_000_000),
    };
    let mut decoder = decoder::Decoder::new()?;
    let mut link = tokio::process::Command::new("pw-link")
        .arg("--wait")
        .arg("--remote")
        .arg(socket)
        .arg(format!("{}:capture_1", identity.node_name))
        .arg("pronk-backend-media-1:input_1")
        .kill_on_drop(true)
        .spawn()
        .context("start encoder port link")?;
    let mut ready = BTreeSet::new();
    let mut published = 0u64;
    let mut received = 0u64;
    let mut decoded = 0u64;
    let mut changed = 0u64;
    let mut first_timestamp = None;
    let mut last_encoded_timestamp = None;
    let mut tick = tokio::time::interval(Duration::from_millis(34));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut diagnostics = tokio::time::interval(Duration::from_secs(2));
    {
        let mut started = false;
        let activation = async {
            media.configure(configuration).await?;
            media.start(identity.media_generation).await
        };
        tokio::pin!(activation);
        while decoded < 12 || changed == 0 || !started {
            tokio::select! {
                result = &mut activation, if !started => { result?; started = true; }
            _ = diagnostics.tick() => {
                decoder.check()?;
                let snapshot = media.snapshot();
                ensure!(snapshot.state != pronk_media::MediaGraphState::Failed, "media failed: {:?}", snapshot.last_error);
                eprintln!("ready={} published={published} encoded={received} decoded={decoded} state={:?}", ready.len(), snapshot.state);
                }
                event = source.next_event() => {
                    let event = event.context("source stopped")?;
                    if let VideoSourceActorEvent::GenerationFailed { error, .. } = &event { anyhow::bail!("source failed: {error}"); }
                    if let VideoSourceActorEvent::BufferAvailable { buffer_id, .. } = &event { ready.insert(*buffer_id); }
                    output.handle_event(&event)?;
                }
                frame = encoded.recv() => {
                    let frame = frame.context("encoded output stopped")?;
                    if received == 0 { ensure!(frame.dependency == VideoFrameDependency::KeyFrame, "first access unit is not a key frame"); }
                ensure!(frame.media_generation == identity.media_generation && !frame.data.is_empty(), "invalid encoded identity");
                ensure!(!frame.duration.is_zero() && last_encoded_timestamp.is_none_or(|last| frame.media_timestamp > last), "invalid encoded timing");
                last_encoded_timestamp = Some(frame.media_timestamp);
                    decoder.push(frame)?;
                    received += 1;
                }
                pixels = decoder.next(640, 480) => {
                    let pixels = pixels?;
                    decoded += 1;
                if decoded == 1 { ensure!(pixels == 0x49, "incorrect first decoded image"); }
                if changed > 0 { ensure!(pixels == 0x68, "old content followed changed content"); }
                    if decoded == 3 { fixture.flip(); }
                    if pixels == 0x68 { changed += 1; }
                }
                _ = tick.tick(), if ready.len() == 3 && published < 60 => {
                    match capture.capture().await {
                        Ok(frame) => {
                            let origin = *first_timestamp.get_or_insert(frame.timestamp());
                        let pts = i64::try_from(frame.timestamp().checked_sub(origin).context("capture timestamp regressed")?.as_nanos())?;
                            let video = output.begin_publish(frame, pts, published == 0)?;
                            source.publish(identity.media_generation, video).await?;
                            published += 1;
                        }
                        Err(CaptureError::Backpressure) => (),
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }
    let statistics = media.stop(identity.media_generation).await?;
    eprintln!("published={published} encoded={received} decoded={decoded} changed={changed} statistics={statistics:?}");
    media.shutdown().await?;
    drop(decoder);
    let report = source.stop(identity.media_generation).await?;
    output.stopped(&report)?;
    source.shutdown().await?;
    drop(output);
    drop(capture.shutdown().await?);
    drop(control);
    ensure!(link.wait().await?.success(), "encoder port link failed");
    Ok(())
}
