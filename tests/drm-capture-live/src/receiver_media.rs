use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk::media_pipeline_port::CapturePipelinePort;
use pronk::media_session::{MediaStartRequest, MediaStopReason};
use pronk_capture_receiver_test::{Receiver, SenderEvent};
use pronk_media::{
    MediaGraphActor, MediaGraphConfiguration, PipeWireVideoInput, ValidatedVideoCaps, VideoCadence,
    VideoEncoder, VideoFrameDependency, VideoInputLayout,
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
    encoder: VideoEncoder,
) -> anyhow::Result<()> {
    let generation = NonZeroU64::new(request.media_generation).context("zero media generation")?;
    let width = request.route.mode.width;
    let height = request.route.mode.height;
    let prepared = capture.start(request, CancellationToken::new()).await?;
    let video_target = prepared.video_target;
    let cadence = VideoCadence::new(nz(30), nz(1));
    let bitrate = nz64(4_000_000);
    let va_render_node = encoder.render_node().map(Path::to_path_buf);
    if let Some(render_node) = va_render_node.as_deref() {
        qualify_va_target(
            &video_target,
            &encoder,
            render_node,
            width,
            height,
            cadence,
            bitrate,
        )?;
    }
    if let Some(address) = address {
        eprintln!(
            "Starting an explicit receiver test at {address}; current playback will be interrupted"
        );
        receiver.start(address, width, height).await?;
    }

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
        video_encoder: encoder,
        video_cadence: cadence,
        video_bitrate: bitrate,
    };
    let mut decoder = Some(Decoder::new()?);
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
            let check_pixels = decoded < 12 || colors.len() < 2;
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
                    if check_pixels {
                        if address.is_some() {
                            receiver.send(frame.clone()).await?;
                        }
                        decoder
                            .as_ref()
                            .context("pixel oracle stopped before its sample was complete")?
                            .push(frame)?;
                    } else if address.is_some() {
                        receiver.send(frame).await?;
                    }
                    received += 1;
                }
                pixels = async {
                    decoder
                        .as_mut()
                        .context("pixel oracle stopped before its sample was complete")?
                        .next(width, height)
                        .await
                }, if check_pixels => {
                    colors.insert(pixels?);
                    decoded += 1;
                    if decoded >= 12 && colors.len() >= 2 {
                        drop(decoder.take());
                    }
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
                    if let Some(decoder) = decoder.as_ref() {
                        decoder.check()?;
                    }
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
    let statistics = media.stop(generation).await?;
    media.shutdown().await?;
    verify_encoded_delivery(&statistics)?;
    if let Some(render_node) = va_render_node.as_deref() {
        verify_va_execution(&statistics, render_node)?;
    }
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

fn verify_encoded_delivery(statistics: &pronk_media::MediaGraphStatistics) -> anyhow::Result<()> {
    ensure!(
        statistics.dropped_frames == 0,
        "media graph discarded {} encoded access units",
        statistics.dropped_frames
    );
    Ok(())
}

fn verify_va_execution(
    statistics: &pronk_media::MediaGraphStatistics,
    selected_node: &Path,
) -> anyhow::Result<()> {
    ensure!(
        statistics
            .encoder_name
            .as_deref()
            .is_some_and(|name| name.starts_with("va") && name.ends_with("h264enc"))
            && statistics.video_memory_path.as_deref() == Some("DMA-BUF DMA_DRM to VA-memory NV12"),
        "live media graph did not use the selected VA H.264 path: {statistics:?}"
    );
    let reported = statistics
        .render_device
        .as_deref()
        .context("live VA media graph did not report its render device")?;
    ensure!(
        std::fs::metadata(reported)?.rdev() == std::fs::metadata(selected_node)?.rdev(),
        "live VA media graph used {reported}, not {}",
        selected_node.display()
    );
    Ok(())
}

fn qualify_va_target(
    video_target: &pronk::device_session_port::DeviceMediaTarget,
    encoder: &VideoEncoder,
    render_node: &Path,
    width: u32,
    height: u32,
    cadence: VideoCadence,
    bitrate: NonZeroU64,
) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(render_node)
        .with_context(|| format!("inspect selected render device {}", render_node.display()))?;
    ensure!(
        metadata.file_type().is_char_device(),
        "selected VA render device is not a character device"
    );
    let device = video_target
        .render_device
        .context("renderer did not identify its render device")?;
    ensure!(
        u64::from(device.major) == nix::sys::stat::major(metadata.rdev())
            && u64::from(device.minor) == nix::sys::stat::minor(metadata.rdev()),
        "selected VA render device differs from the renderer's Vulkan device"
    );
    let caps = ValidatedVideoCaps::parse(&video_target.caps)?;
    ensure!(
        caps.width.get() == width && caps.height.get() == height && caps.supports_cadence(cadence),
        "renderer video target does not match the requested picture and cadence"
    );
    let format = match caps.layout {
        VideoInputLayout::DmaBuf { drm_format } => drm_format,
        VideoInputLayout::SystemMemoryBgrx => {
            anyhow::bail!("VA H.264 requires a DMA-BUF video target")
        }
    };
    let accepted = encoder.supported_dma_buf_formats_for_dimensions(&[(width, height)], cadence)?;
    ensure!(
        accepted.first().is_some_and(|formats| formats.contains(&format)),
        "selected VA converter does not accept the renderer's exact output layout {format:?} at {width}x{height}"
    );
    let (minimum_bitrate, maximum_bitrate) = encoder.bitrate_limits(cadence)?;
    ensure!(
        (minimum_bitrate..=maximum_bitrate).contains(&bitrate.get()),
        "test bitrate is outside the selected VA encoder range {minimum_bitrate}..={maximum_bitrate} bit/s"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{verify_encoded_delivery, verify_va_execution};
    use pronk_media::MediaGraphStatistics;
    use std::path::Path;

    #[test]
    fn live_va_result_requires_the_selected_encoder_path() {
        let selected = Path::new("/dev/null");
        let mut statistics = MediaGraphStatistics {
            encoder_name: Some("vah264enc".into()),
            video_memory_path: Some("DMA-BUF DMA_DRM to VA-memory NV12".into()),
            render_device: Some(selected.display().to_string()),
            ..MediaGraphStatistics::default()
        };
        assert!(verify_va_execution(&statistics, selected).is_ok());

        statistics.encoder_name = Some("x264enc".into());
        assert!(verify_va_execution(&statistics, selected).is_err());
        statistics.encoder_name = Some("vah264enc".into());
        statistics.video_memory_path = Some("system-memory BGRx to system-memory I420".into());
        assert!(verify_va_execution(&statistics, selected).is_err());
        statistics.video_memory_path = Some("DMA-BUF DMA_DRM to VA-memory NV12".into());
        statistics.render_device = Some("/dev/zero".into());
        assert!(verify_va_execution(&statistics, selected).is_err());
    }

    #[test]
    fn encoded_output_loss_fails_the_receiver_probe() {
        let mut statistics = MediaGraphStatistics::default();
        assert!(verify_encoded_delivery(&statistics).is_ok());
        statistics.dropped_frames = 1;
        assert!(verify_encoded_delivery(&statistics).is_err());
    }
}
