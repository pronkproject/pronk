use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk::media_pipeline_port::CapturePipelinePort;
use pronk::media_session::{MediaStartRequest, MediaStopReason};
use pronk_capture_receiver_test::{Receiver, SenderEvent};
use pronk_media::{
    MediaGraphActor, MediaGraphConfiguration, PipeWireVideoInput, VideoCadence, VideoCodec,
    VideoEncoder, VideoFrameDependency,
};
use tokio_util::sync::CancellationToken;

use crate::decoder::Decoder;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

pub async fn run(
    capture: &mut impl CapturePipelinePort,
    request: MediaStartRequest,
    socket: &Path,
    receiver: &mut Receiver,
    address: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let generation = NonZeroU64::new(request.media_generation).context("zero media generation")?;
    let width = request.route.mode.width;
    let height = request.route.mode.height;
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
            node_name: video_target.node_name,
            object_serial: video_target.object_serial,
            caps: video_target.caps,
        },
        audio: None,
        video_encoder: VideoEncoder::software(VideoCodec::H264),
        video_cadence: VideoCadence::new(nz(30), nz(1)),
        video_bitrate: nz64(4_000_000),
    };
    let mut decoder = Decoder::new()?;
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
                    if received == 0 {
                        ensure!(
                            frame.dependency == VideoFrameDependency::KeyFrame,
                            "initial key frame missing"
                        );
                    }
                    ensure!(
                        frame.media_generation == generation
                            && last_timestamp.is_none_or(|last| frame.media_timestamp > last),
                        "encoded identity/timing"
                    );
                    ensure!(
                        !frame.data.is_empty() && !frame.duration.is_zero(),
                        "empty encoded frame or duration"
                    );
                    last_timestamp = Some(frame.media_timestamp);
                    if address.is_some() {
                        receiver.send(frame.clone()).await?;
                    }
                    decoder.push(frame)?;
                    received += 1;
                }
                pixels = decoder.next(width, height) => {
                    colors.insert(pixels?);
                    decoded += 1;
                }
                event = receiver.next_event(), if address.is_some() => {
                    match event? {
                        SenderEvent::NeedsKeyFrame { .. } => media.request_key_frame(generation).await?,
                        SenderEvent::ReceiverTimedOut => {
                            anyhow::bail!("receiver acknowledgements timed out")
                        }
                        SenderEvent::FatalError(error) => return Err(error.into()),
                        _ => (),
                    }
                }
                _ = tick.tick() => {
                    decoder.check()?;
                    let snapshot = media.snapshot();
                    ensure!(
                        snapshot.state != pronk_media::MediaGraphState::Failed,
                        "media failed: {:?}",
                        snapshot.last_error
                    );
                    eprintln!(
                        "Captured {width}x{height} encoded={received} decoded={decoded} colors={colors:?}"
                    );
                    if address.is_some() {
                        let statistics = receiver.statistics().await?;
                        acknowledged = statistics.frames_acked;
                        eprintln!(
                            "receiver acknowledged={acknowledged} in_flight={}",
                            statistics.in_flight_frames
                        );
                        if acknowledged == 0 {
                            media.request_key_frame(generation).await?;
                        }
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
    eprintln!("Encoded={received} decoded={decoded} colors={colors:?}");
    Ok(())
}
