//! Production media-session orchestration over narrow application ports.

use std::future::Future;
use std::num::NonZeroU64;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::device_session_port::{
    DeviceMediaEndpoint, DeviceMediaKind, DeviceMediaSetup, DeviceMediaStopReason,
    DeviceMediaSuspendReason, DeviceSessionPort, DeviceSessionStopReason,
};
use crate::media_pipeline_port::{
    CapturePipelinePort, DeviceMediaRemotePort, MediaPipelineError, PreparedCaptureMedia,
};
use crate::media_session::{
    MediaDriverError, MediaSessionDriver, MediaStartRequest, MediaStopReason, MediaSuspendReason,
};

/// Coordinates one capture owner, one authority-limited remote minter, and one
/// prepared Device session. It contains ordering policy only; infrastructure
/// details stay behind the three ports.
#[derive(Debug)]
pub struct ProductionMediaSessionDriver {
    capture: Box<dyn CapturePipelinePort>,
    remotes: Box<dyn DeviceMediaRemotePort>,
    device: Option<Box<dyn DeviceSessionPort>>,
    capture_generation: Option<NonZeroU64>,
    prepared: Option<PreparedCaptureMedia>,
    backend_generation: Option<NonZeroU64>,
    shutdown: bool,
}

impl ProductionMediaSessionDriver {
    pub fn new(
        capture: Box<dyn CapturePipelinePort>,
        remotes: Box<dyn DeviceMediaRemotePort>,
        device: Box<dyn DeviceSessionPort>,
    ) -> Self {
        Self {
            capture,
            remotes,
            device: Some(device),
            capture_generation: None,
            prepared: None,
            backend_generation: None,
            shutdown: false,
        }
    }

    fn ensure_live(&self) -> Result<(), MediaDriverError> {
        if self.shutdown || self.device.is_none() {
            Err(MediaDriverError::new("media driver is shut down"))
        } else {
            Ok(())
        }
    }

    fn device(&mut self) -> Result<&mut Box<dyn DeviceSessionPort>, MediaDriverError> {
        self.device
            .as_mut()
            .ok_or_else(|| MediaDriverError::new("Device session is no longer available"))
    }
}

#[async_trait]
impl MediaSessionDriver for ProductionMediaSessionDriver {
    async fn start_capture(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError> {
        self.ensure_live()?;
        let generation = nonzero_generation(request.media_generation)?;
        if self.capture_generation.is_some() || self.backend_generation.is_some() {
            return Err(MediaDriverError::new(
                "a previous media generation still requires cleanup",
            ));
        }

        // Mark the generation before awaiting: cancellation or timeout can be
        // ambiguous after the capture owner observes the command.
        self.capture_generation = Some(generation);
        let prepared = cancellable(
            cancellation.clone(),
            "start capture pipeline",
            self.capture.start(request, cancellation),
        )
        .await?;
        validate_prepared_capture(&prepared, request)?;
        self.prepared = Some(prepared);
        Ok(())
    }

    async fn start_media(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError> {
        self.ensure_live()?;
        let generation = nonzero_generation(request.media_generation)?;
        if self.capture_generation != Some(generation) {
            return Err(generation_mismatch(
                "start backend media",
                self.capture_generation,
                generation,
            ));
        }
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| MediaDriverError::new("capture did not return media targets"))?;
        let needs_audio = prepared.audio_target.is_some();
        let remote_set = cancellable(
            cancellation.clone(),
            "mint backend PipeWire remotes",
            self.remotes
                .mint(generation, needs_audio, cancellation.clone()),
        )
        .await?;
        if remote_set.audio.is_some() != needs_audio {
            return Err(MediaDriverError::new(
                "PipeWire remote layout differs from prepared media targets",
            ));
        }

        let prepared = self
            .prepared
            .take()
            .expect("prepared capture was checked above");
        let mut endpoints = vec![DeviceMediaEndpoint {
            remote: remote_set.video,
            target: prepared.video_target,
        }];
        if let (Some(remote), Some(target)) = (remote_set.audio, prepared.audio_target) {
            endpoints.push(DeviceMediaEndpoint { remote, target });
        }
        let setup = DeviceMediaSetup {
            media_generation: generation,
            endpoints,
            configuration: prepared.configuration,
        };

        // ConfigureMedia transfers authority. Any interrupted/error reply is
        // ambiguous and must be followed by generation-matched StopMedia.
        self.backend_generation = Some(generation);
        cancellable(
            cancellation.clone(),
            "configure backend media",
            self.device()?.configure_media(setup),
        )
        .await?;
        cancellable(
            cancellation.clone(),
            "activate capture pipeline",
            self.capture.activate(generation, cancellation.clone()),
        )
        .await?;
        cancellable(
            cancellation,
            "start backend media",
            self.device()?.start_media(generation),
        )
        .await
    }

    async fn suspend(
        &mut self,
        media_generation: u64,
        reason: MediaSuspendReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError> {
        self.ensure_live()?;
        let generation = nonzero_generation(media_generation)?;
        if self.backend_generation != Some(generation)
            || self.capture_generation != Some(generation)
        {
            return Err(generation_mismatch(
                "suspend media",
                self.backend_generation.or(self.capture_generation),
                generation,
            ));
        }
        cancellable(
            cancellation.clone(),
            "suspend backend media",
            self.device()?
                .suspend_media(generation, map_suspend_reason(reason)),
        )
        .await?;
        cancellable(
            cancellation.clone(),
            "suspend capture pipeline",
            self.capture.suspend(generation, reason, cancellation),
        )
        .await
    }

    async fn stop(
        &mut self,
        media_generation: u64,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError> {
        let generation = nonzero_generation(media_generation)?;
        let mut failures = Vec::new();

        if let Some(active) = self.backend_generation {
            if active != generation {
                failures.push(
                    generation_mismatch("stop backend media", Some(active), generation).to_string(),
                );
            } else if self.device.is_none() {
                // Cleanup is best-effort across independent authorities. A
                // missing backend owner must not skip capture teardown.
                failures.push("Device session is no longer available".into());
            } else {
                let result = cancellable(
                    cancellation.clone(),
                    "stop backend media",
                    self.device()?
                        .stop_media(generation, map_stop_reason(reason)),
                )
                .await;
                match result {
                    Ok(()) => self.backend_generation = None,
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }

        self.prepared = None;
        if let Some(active) = self.capture_generation {
            if active != generation {
                failures.push(
                    generation_mismatch("stop capture pipeline", Some(active), generation)
                        .to_string(),
                );
            } else {
                let result = cancellable(
                    cancellation.clone(),
                    "stop capture pipeline",
                    self.capture.stop(generation, reason, cancellation),
                )
                .await;
                match result {
                    Ok(()) => self.capture_generation = None,
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }

        combine_failures(failures)
    }

    async fn shutdown(
        &mut self,
        reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError> {
        if self.shutdown {
            return Ok(());
        }
        let mut failures = Vec::new();

        let final_reason = match reason {
            MediaStopReason::DisplayRemoved => DeviceSessionStopReason::DisplayRemoved,
            _ => DeviceSessionStopReason::DaemonShutdown,
        };
        let device = self.device.take();
        let stop_device = async move {
            match device {
                Some(device) => device
                    .stop(final_reason)
                    .await
                    .map_err(|error| format!("final Device-session stop failed: {error}")),
                None => Ok(()),
            }
        };
        // These are independent resource owners. A wedged backend teardown
        // must not keep the capture/PipeWire owner from beginning its own
        // final cleanup before the outer actor deadline expires.
        let (device_result, capture_result) = tokio::join!(
            stop_device,
            cancellable(
                cancellation.clone(),
                "shut down capture pipeline",
                self.capture.shutdown(reason, cancellation),
            )
        );
        if let Err(error) = device_result {
            failures.push(error);
        }
        if let Err(error) = capture_result {
            failures.push(error.to_string());
        }
        self.prepared = None;
        self.backend_generation = None;
        self.capture_generation = None;
        self.shutdown = true;
        combine_failures(failures)
    }
}

fn validate_prepared_capture(
    prepared: &PreparedCaptureMedia,
    request: MediaStartRequest,
) -> Result<(), MediaDriverError> {
    let generation = nonzero_generation(request.media_generation)?;
    if prepared.media_generation != generation
        || prepared.video_target.media_generation != generation
        || prepared
            .audio_target
            .as_ref()
            .is_some_and(|target| target.media_generation != generation)
    {
        return Err(MediaDriverError::new(
            "capture pipeline returned a stale media generation",
        ));
    }
    if prepared.video_target.kind != DeviceMediaKind::Video
        || prepared
            .audio_target
            .as_ref()
            .is_some_and(|target| target.kind != DeviceMediaKind::Audio)
    {
        return Err(MediaDriverError::new(
            "capture pipeline returned an invalid target ordering",
        ));
    }
    if prepared.audio_target.is_some() != prepared.configuration.audio_profile_id.is_some() {
        return Err(MediaDriverError::new(
            "capture targets and negotiated audio profile disagree",
        ));
    }
    if prepared.configuration.mode != request.route.mode {
        return Err(MediaDriverError::new(
            "capture media mode differs from the active kernel route",
        ));
    }
    Ok(())
}

fn nonzero_generation(media_generation: u64) -> Result<NonZeroU64, MediaDriverError> {
    NonZeroU64::new(media_generation)
        .ok_or_else(|| MediaDriverError::new("media generation must be nonzero"))
}

fn generation_mismatch(
    operation: &'static str,
    active: Option<NonZeroU64>,
    requested: NonZeroU64,
) -> MediaDriverError {
    MediaDriverError::new(format!(
        "{operation} requested generation {requested}; active generation is {active:?}"
    ))
}

fn map_suspend_reason(reason: MediaSuspendReason) -> DeviceMediaSuspendReason {
    match reason {
        MediaSuspendReason::GrantUnavailable => DeviceMediaSuspendReason::SessionInactive,
        MediaSuspendReason::DeviceUnavailable => DeviceMediaSuspendReason::DeviceUnavailable,
        MediaSuspendReason::SessionInactive => DeviceMediaSuspendReason::SessionInactive,
    }
}

fn map_stop_reason(reason: MediaStopReason) -> DeviceMediaStopReason {
    match reason {
        MediaStopReason::OutputDisabled => DeviceMediaStopReason::OutputDisabled,
        MediaStopReason::ModeChanged => DeviceMediaStopReason::ModeChanged,
        MediaStopReason::DisplayRemoved => DeviceMediaStopReason::DisplayRemoved,
        MediaStopReason::BackendShutdown => DeviceMediaStopReason::BackendShutdown,
        MediaStopReason::TransportFailure => DeviceMediaStopReason::TransportFailure,
    }
}

async fn cancellable<T, E, F>(
    cancellation: CancellationToken,
    operation: &'static str,
    future: F,
) -> Result<T, MediaDriverError>
where
    E: std::fmt::Display,
    F: Future<Output = Result<T, E>>,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            Err(MediaDriverError::new(format!("{operation} was cancelled")))
        }
        result = future => result.map_err(|error| {
            MediaDriverError::new(format!("{operation} failed: {error}"))
        }),
    }
}

fn combine_failures(failures: Vec<String>) -> Result<(), MediaDriverError> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(MediaDriverError::new(failures.join("; ")))
    }
}

impl From<MediaPipelineError> for MediaDriverError {
    fn from(error: MediaPipelineError) -> Self {
        Self::new(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::device_session_port::{
        DeviceMediaConfiguration, DeviceMediaTarget, DeviceSessionError,
    };
    use crate::display_state::{RouteTarget, RoutedMode};
    use crate::media_pipeline_port::DeviceMediaRemoteSet;
    use crate::media_session::MediaRoute;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        CaptureStart(u64),
        Mint(u64, bool),
        Configure(u64),
        CaptureActivate(u64),
        DeviceStart(u64),
        DeviceSuspend(u64),
        CaptureSuspend(u64),
        DeviceStopMedia(u64),
        CaptureStop(u64),
        DeviceFinalStop(DeviceSessionStopReason),
        CaptureShutdown,
    }

    type Calls = Arc<Mutex<Vec<Call>>>;

    #[derive(Debug)]
    struct FakeCapture {
        calls: Calls,
        returned_generation: u64,
    }

    #[async_trait]
    impl CapturePipelinePort for FakeCapture {
        async fn start(
            &mut self,
            request: MediaStartRequest,
            _cancellation: CancellationToken,
        ) -> Result<PreparedCaptureMedia, MediaPipelineError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::CaptureStart(request.media_generation));
            let generation = NonZeroU64::new(self.returned_generation).unwrap();
            Ok(PreparedCaptureMedia {
                media_generation: generation,
                video_target: target(DeviceMediaKind::Video, generation),
                audio_target: None,
                configuration: DeviceMediaConfiguration {
                    video_profile_id: "h264-high".into(),
                    audio_profile_id: None,
                    mode: request.route.mode,
                    video_bitrate: NonZeroU64::new(8_000_000).unwrap(),
                },
            })
        }

        async fn activate(
            &mut self,
            media_generation: NonZeroU64,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaPipelineError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::CaptureActivate(media_generation.get()));
            Ok(())
        }

        async fn suspend(
            &mut self,
            media_generation: NonZeroU64,
            _reason: MediaSuspendReason,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaPipelineError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::CaptureSuspend(media_generation.get()));
            Ok(())
        }

        async fn stop(
            &mut self,
            media_generation: NonZeroU64,
            _reason: MediaStopReason,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaPipelineError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::CaptureStop(media_generation.get()));
            Ok(())
        }

        async fn shutdown(
            &mut self,
            _reason: MediaStopReason,
            _cancellation: CancellationToken,
        ) -> Result<(), MediaPipelineError> {
            self.calls.lock().unwrap().push(Call::CaptureShutdown);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeRemotes {
        calls: Calls,
    }

    #[async_trait]
    impl DeviceMediaRemotePort for FakeRemotes {
        async fn mint(
            &mut self,
            media_generation: NonZeroU64,
            needs_audio: bool,
            _cancellation: CancellationToken,
        ) -> Result<DeviceMediaRemoteSet, MediaPipelineError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Mint(media_generation.get(), needs_audio));
            let (video, _peer) = UnixStream::pair().unwrap();
            Ok(DeviceMediaRemoteSet {
                video: OwnedFd::from(video),
                audio: None,
            })
        }
    }

    #[derive(Debug)]
    struct FakeDevice {
        calls: Calls,
        block_final_stop: bool,
    }

    #[async_trait]
    impl DeviceSessionPort for FakeDevice {
        async fn configure_media(
            &mut self,
            setup: DeviceMediaSetup,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Configure(setup.media_generation.get()));
            Ok(())
        }

        async fn start_media(
            &mut self,
            media_generation: NonZeroU64,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::DeviceStart(media_generation.get()));
            Ok(())
        }

        async fn suspend_media(
            &mut self,
            media_generation: NonZeroU64,
            _reason: DeviceMediaSuspendReason,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::DeviceSuspend(media_generation.get()));
            Ok(())
        }

        async fn resume_media(
            &mut self,
            _media_generation: NonZeroU64,
        ) -> Result<(), DeviceSessionError> {
            Ok(())
        }

        async fn stop_media(
            &mut self,
            media_generation: NonZeroU64,
            _reason: DeviceMediaStopReason,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::DeviceStopMedia(media_generation.get()));
            Ok(())
        }

        async fn stop(
            self: Box<Self>,
            reason: DeviceSessionStopReason,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::DeviceFinalStop(reason));
            if self.block_final_stop {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }

    fn target(kind: DeviceMediaKind, generation: NonZeroU64) -> DeviceMediaTarget {
        DeviceMediaTarget {
            kind,
            node_name: format!("pronk.video.test.{generation}"),
            object_serial: NonZeroU64::new(101).unwrap(),
            session_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            device_instance: "test-device".into(),
            connector_id: NonZeroU32::new(40).unwrap(),
            output_index: 0,
            media_generation: generation,
            caps: "video/x-raw,format=BGRx".into(),
        }
    }

    fn request(generation: u64) -> MediaStartRequest {
        MediaStartRequest {
            media_generation: generation,
            route: MediaRoute {
                route_generation: 1,
                target: RouteTarget::new(NonZeroU32::new(7).unwrap()),
                mode: RoutedMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            },
        }
    }

    fn driver(calls: &Calls, returned_generation: u64) -> ProductionMediaSessionDriver {
        ProductionMediaSessionDriver::new(
            Box::new(FakeCapture {
                calls: calls.clone(),
                returned_generation,
            }),
            Box::new(FakeRemotes {
                calls: calls.clone(),
            }),
            Box::new(FakeDevice {
                calls: calls.clone(),
                block_final_stop: false,
            }),
        )
    }

    #[tokio::test]
    async fn orders_authority_transfer_and_teardown_across_narrow_ports() {
        let calls = Calls::default();
        let mut driver = driver(&calls, 1);
        let cancellation = CancellationToken::new();
        driver
            .start_capture(request(1), cancellation.clone())
            .await
            .unwrap();
        driver
            .start_media(request(1), cancellation.clone())
            .await
            .unwrap();
        driver
            .suspend(1, MediaSuspendReason::SessionInactive, cancellation.clone())
            .await
            .unwrap();
        driver
            .stop(1, MediaStopReason::OutputDisabled, cancellation.clone())
            .await
            .unwrap();
        driver
            .shutdown(MediaStopReason::DisplayRemoved, cancellation)
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::CaptureStart(1),
                Call::Mint(1, false),
                Call::Configure(1),
                Call::CaptureActivate(1),
                Call::DeviceStart(1),
                Call::DeviceSuspend(1),
                Call::CaptureSuspend(1),
                Call::DeviceStopMedia(1),
                Call::CaptureStop(1),
                Call::DeviceFinalStop(DeviceSessionStopReason::DisplayRemoved),
                Call::CaptureShutdown,
            ]
        );
    }

    #[tokio::test]
    async fn stale_capture_output_never_mints_or_transfers_a_remote() {
        let calls = Calls::default();
        let mut driver = driver(&calls, 2);
        let cancellation = CancellationToken::new();
        assert!(driver
            .start_capture(request(1), cancellation.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("stale media generation"));
        driver
            .stop(1, MediaStopReason::TransportFailure, cancellation.clone())
            .await
            .unwrap();
        driver
            .shutdown(MediaStopReason::BackendShutdown, cancellation)
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::CaptureStart(1),
                Call::CaptureStop(1),
                Call::DeviceFinalStop(DeviceSessionStopReason::DaemonShutdown),
                Call::CaptureShutdown,
            ]
        );
    }

    #[tokio::test]
    async fn missing_device_owner_does_not_skip_capture_cleanup() {
        let calls = Calls::default();
        let mut driver = driver(&calls, 1);
        let cancellation = CancellationToken::new();
        driver
            .start_capture(request(1), cancellation.clone())
            .await
            .unwrap();
        driver
            .start_media(request(1), cancellation.clone())
            .await
            .unwrap();

        drop(driver.device.take());
        let error = driver
            .stop(1, MediaStopReason::TransportFailure, cancellation.clone())
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Device session is no longer available"));
        assert_eq!(driver.capture_generation, None);
        assert_eq!(driver.backend_generation, NonZeroU64::new(1));
        assert_eq!(calls.lock().unwrap().last(), Some(&Call::CaptureStop(1)));

        driver
            .shutdown(MediaStopReason::BackendShutdown, cancellation)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wedged_final_device_stop_does_not_skip_capture_owner_shutdown() {
        let calls = Calls::default();
        let mut driver = ProductionMediaSessionDriver::new(
            Box::new(FakeCapture {
                calls: calls.clone(),
                returned_generation: 1,
            }),
            Box::new(FakeRemotes {
                calls: calls.clone(),
            }),
            Box::new(FakeDevice {
                calls: calls.clone(),
                block_final_stop: true,
            }),
        );

        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            driver.shutdown(MediaStopReason::BackendShutdown, CancellationToken::new(),),
        )
        .await
        .is_err());
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&Call::DeviceFinalStop(
            DeviceSessionStopReason::DaemonShutdown
        )));
        assert!(calls.contains(&Call::CaptureShutdown));
    }
}
