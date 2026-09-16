//! Application capture generations against an unused display and private test server.

#[allow(dead_code)]
mod fixture;
mod pipewire_consumer;

use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use drm_capture::Access;
use pronk::display_state::{RouteTarget, RoutedMode};
use pronk::drm_capture_pipeline::{DrmCapturePipeline, DrmCapturePipelineConfig};
use pronk::media_pipeline_port::{CaptureEvent, CaptureEventPort, CapturePipelinePort};
use pronk::media_session::{MediaRoute, MediaStartRequest, MediaStopReason};
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
    let mut args = std::env::args_os().skip(1);
    let device = PathBuf::from(args.next().context("expected unused VM DRM device")?);
    let socket = PathBuf::from(
        args.next()
            .context("expected private test PipeWire socket")?,
    );
    ensure!(args.next().is_none(), "expected device and socket only");
    tokio::time::timeout(Duration::from_secs(40), run(&device, &socket)).await??;
    println!("PASS: application capture, media-service restart, revocation and authority recovery");
    Ok(())
}

async fn run(device: &Path, socket: &Path) -> anyhow::Result<()> {
    let mut fixture = fixture::Fixture::open(device)?;
    let (client, control) = drm_capture::create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let runtime = socket.parent().context("private socket has no directory")?;
    let remotes =
        ClassifiedSocketRemoteProvider::new(ClassifiedSocketPaths::in_runtime_dir(runtime)?);
    let config = DrmCapturePipelineConfig {
        connector_id: nz(fixture.connector()),
        output_index: 0,
        session_id: "capture-pipeline-test".into(),
        device_instance: "castkms-test".into(),
        node_description: "Application capture test".into(),
        video_profile_id: "h264".into(),
        video_bitrate: nz64(2_000_000),
        capture_rate_hz: nz(30),
        pool_size: nz(4),
        request_capacity: nz(3),
        pool_byte_limit: nz64(16 * 1024 * 1024),
        heap_path: "/dev/dma_heap/system".into(),
        poll_interval: Duration::from_millis(2),
        shutdown_timeout: Duration::from_secs(5),
    };
    let (mut capture, mut events) = DrmCapturePipeline::new(
        Access::from_fd(client.into_owner()),
        remotes.clone(),
        config.clone(),
    );
    let generations = tokio::time::timeout(Duration::from_secs(25), async {
        let mut retained = None;
        for number in 1..=3 {
            let generation = nz64(number);
            let request = capture_request(&fixture, number);
            let prepared = capture.start(request, CancellationToken::new()).await?;
            ensure!(
                prepared.media_generation == generation,
                "stale capture generation"
            );
            ensure!(
                prepared.audio_target.is_none(),
                "unexpected audio capability"
            );
            let node = &prepared.video_target.node_name;
            let mut consumer = pipewire_consumer::Consumer::start(socket, node, true)?;
            ensure!(
                tokio::time::timeout(Duration::from_millis(100), consumer.next())
                    .await
                    .is_err(),
                "capture published before activation"
            );
            capture
                .activate(generation, CancellationToken::new())
                .await?;
            let expected = if number == 1 { 0x49 } else { 0x68 };
            for sequence in 1..=12 {
                consumer.check()?;
                let sample = tokio::select! {
                    sample = consumer.next() => sample?,
                    failure = events.next_event() => bail!("capture health event: {failure:?}"),
                };
                ensure!(
                    pipewire_consumer::check_pixels(&sample, expected)? == sequence,
                    "missing or reordered frame"
                );
                if let Some(first) = &retained {
                    pipewire_consumer::check_pixels(first, 0x49)?;
                }
                if number == 1 && sequence == 1 {
                    retained = Some(sample);
                }
            }
            drop(consumer);
            capture
                .stop(
                    generation,
                    MediaStopReason::ModeChanged,
                    CancellationToken::new(),
                )
                .await?;
            ensure!(
                tokio::time::timeout(Duration::from_millis(50), events.next_event())
                    .await
                    .is_err(),
                "ordinary capture stop reported failure"
            );
            if number == 1 {
                fixture.flip();
            }
        }
        pipewire_consumer::check_pixels(retained.as_ref().context("retained first frame")?, 0x49)?;
        Ok::<_, anyhow::Error>(retained)
    })
    .await
    .context("capture generations timed out")
    .and_then(|result| result);

    let service_restart = if generations.is_ok() {
        async {
            let generation = nz64(4);
            let prepared = capture
                .start(
                    capture_request(&fixture, generation.get()),
                    CancellationToken::new(),
                )
                .await?;
            let mut consumer =
                pipewire_consumer::Consumer::start(socket, &prepared.video_target.node_name, true)?;
            capture
                .activate(generation, CancellationToken::new())
                .await?;
            let disconnected_sample = consumer.next().await?;
            pipewire_consumer::check_pixels(&disconnected_sample, 0x68)?;
            std::fs::write(runtime.join("restart-request"), b"")?;
            let failure = tokio::time::timeout(Duration::from_secs(5), events.next_event())
                .await
                .context("media-service failure was not reported")?
                .context("capture event channel closed after media-service failure")?;
            ensure!(
                matches!(
                    failure,
                    CaptureEvent::Failed {
                        media_generation,
                        ref error,
                    } if media_generation == generation && !error.is_empty()
                ),
                "incorrect media-service failure event: {failure:?}"
            );
            ensure!(
                capture
                    .stop(
                        generation,
                        MediaStopReason::TransportFailure,
                        CancellationToken::new(),
                    )
                    .await
                    .is_err(),
                "disconnected capture reported successful shutdown"
            );
            drop(consumer);
            pipewire_consumer::check_pixels(&disconnected_sample, 0x68)?;
            std::fs::write(runtime.join("restart-observed"), b"")?;
            wait_for_marker(&runtime.join("restart-ready")).await?;

            fixture.restore();
            let generation = nz64(5);
            let prepared = capture
                .start(
                    capture_request(&fixture, generation.get()),
                    CancellationToken::new(),
                )
                .await?;
            let mut consumer =
                pipewire_consumer::Consumer::start(socket, &prepared.video_target.node_name, true)?;
            capture
                .activate(generation, CancellationToken::new())
                .await?;
            for sequence in 1..=6 {
                let sample = tokio::select! {
                    sample = consumer.next() => sample?,
                    failure = events.next_event() => {
                        bail!("capture health event after media-service restart: {failure:?}")
                    }
                };
                ensure!(
                    pipewire_consumer::check_pixels(&sample, 0x49)? == sequence,
                    "missing or reordered frame after media-service restart"
                );
                pipewire_consumer::check_pixels(&disconnected_sample, 0x68)?;
            }
            drop(consumer);
            capture
                .stop(
                    generation,
                    MediaStopReason::DisplayRemoved,
                    CancellationToken::new(),
                )
                .await?;
            drop(disconnected_sample);
            Ok::<_, anyhow::Error>(())
        }
        .await
    } else {
        Ok(())
    };

    let stopped = tokio::time::timeout(
        Duration::from_secs(8),
        capture.shutdown(MediaStopReason::DisplayRemoved, CancellationToken::new()),
    )
    .await
    .context("capture shutdown timed out")
    .and_then(|result| result.map_err(Into::into));
    drop(capture);
    if let Err(error) = &stopped {
        eprintln!("Capture cleanup failed: {error:#}");
    }
    let retained = generations?;
    service_restart?;
    stopped?;
    ensure!(events.next_event().await.is_none(), "late capture failure");

    fixture.flip();
    let (client, revoke_control) = drm_capture::create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let (mut revoked, mut revoked_events) = DrmCapturePipeline::new(
        Access::from_fd(client.into_owner()),
        remotes.clone(),
        config.clone(),
    );
    let generation = nz64(4);
    let prepared = revoked
        .start(
            capture_request(&fixture, generation.get()),
            CancellationToken::new(),
        )
        .await?;
    let mut consumer =
        pipewire_consumer::Consumer::start(socket, &prepared.video_target.node_name, true)?;
    revoked
        .activate(generation, CancellationToken::new())
        .await?;
    let revoked_sample = consumer.next().await?;
    pipewire_consumer::check_pixels(&revoked_sample, 0x68)?;
    drop(revoke_control);
    let failure = tokio::time::timeout(Duration::from_secs(5), revoked_events.next_event())
        .await
        .context("revocation failure was not reported")?
        .context("capture event channel closed during revocation")?;
    ensure!(
        matches!(
            failure,
            CaptureEvent::Failed {
                media_generation,
                ref error,
            } if media_generation == generation && !error.is_empty()
        ),
        "incorrect revocation event: {failure:?}"
    );
    ensure!(
        revoked
            .stop(
                generation,
                MediaStopReason::TransportFailure,
                CancellationToken::new(),
            )
            .await
            .is_err(),
        "revoked capture reported successful shutdown"
    );
    ensure!(
        tokio::time::timeout(Duration::from_millis(50), revoked_events.next_event())
            .await
            .is_err(),
        "revocation reported more than one failure"
    );
    pipewire_consumer::check_pixels(&revoked_sample, 0x68)?;
    pipewire_consumer::check_pixels(retained.as_ref().context("retained first frame")?, 0x49)?;
    drop(consumer);
    drop(revoked);
    ensure!(
        revoked_events.next_event().await.is_none(),
        "late revoked capture failure"
    );

    fixture.restore();
    let (client, recovery_control) = drm_capture::create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let (mut recovery, mut recovery_events) =
        DrmCapturePipeline::new(Access::from_fd(client.into_owner()), remotes, config);
    let generation = nz64(5);
    let prepared = recovery
        .start(
            capture_request(&fixture, generation.get()),
            CancellationToken::new(),
        )
        .await?;
    let mut consumer =
        pipewire_consumer::Consumer::start(socket, &prepared.video_target.node_name, true)?;
    recovery
        .activate(generation, CancellationToken::new())
        .await?;
    for sequence in 1..=6 {
        let sample = tokio::select! {
            sample = consumer.next() => sample?,
            failure = recovery_events.next_event() => {
                bail!("recovered capture health event: {failure:?}")
            }
        };
        ensure!(
            pipewire_consumer::check_pixels(&sample, 0x49)? == sequence,
            "missing or reordered recovery frame"
        );
        pipewire_consumer::check_pixels(&revoked_sample, 0x68)?;
        pipewire_consumer::check_pixels(retained.as_ref().context("retained first frame")?, 0x49)?;
    }
    drop(consumer);
    recovery
        .stop(
            generation,
            MediaStopReason::DisplayRemoved,
            CancellationToken::new(),
        )
        .await?;
    recovery
        .shutdown(MediaStopReason::DisplayRemoved, CancellationToken::new())
        .await?;
    drop(recovery);
    ensure!(
        recovery_events.next_event().await.is_none(),
        "recovered capture reported failure"
    );
    drop(recovery_control);
    drop(revoked_sample);
    drop(retained);
    drop(control);
    Ok(())
}

fn capture_request(fixture: &fixture::Fixture, media_generation: u64) -> MediaStartRequest {
    MediaStartRequest {
        media_generation,
        route: MediaRoute {
            route_generation: 1,
            target: RouteTarget::new(nz(fixture.crtc())),
            mode: RoutedMode {
                width: 640,
                height: 480,
                refresh_millihz: 60_000,
                flags: 0,
            },
        },
    }
}

async fn wait_for_marker(path: &Path) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for {}", path.display()))
}
