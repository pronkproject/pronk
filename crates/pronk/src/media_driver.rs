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
    // Final shutdown moves the Device owner before awaiting both cleanup
    // paths. Cancellation can leave capture cleanup pending without a Device.
    device: Option<Box<dyn DeviceSessionPort>>,
    lifecycle: DriverLifecycle,
}

#[derive(Debug)]
enum DriverLifecycle {
    Live(DriverMediaPhase),
    Shutdown,
}

#[derive(Debug)]
enum DriverMediaPhase {
    Idle,
    StartingCapture(NonZeroU64),
    Prepared(PreparedCaptureMedia),
    BackendPending(NonZeroU64),
    Cleanup {
        generation: NonZeroU64,
        remaining: CleanupRemaining,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupRemaining {
    Capture,
    Backend,
    Both,
}

impl CleanupRemaining {
    fn capture(self) -> bool {
        matches!(self, Self::Capture | Self::Both)
    }

    fn backend(self) -> bool {
        matches!(self, Self::Backend | Self::Both)
    }

    fn without_capture(self) -> Option<Self> {
        match self {
            Self::Capture => None,
            Self::Backend | Self::Both => Some(Self::Backend),
        }
    }

    fn without_backend(self) -> Option<Self> {
        match self {
            Self::Backend => None,
            Self::Capture | Self::Both => Some(Self::Capture),
        }
    }
}

impl DriverMediaPhase {
    fn capture_generation(&self) -> Option<NonZeroU64> {
        match self {
            Self::Idle => None,
            Self::StartingCapture(generation) | Self::BackendPending(generation) => {
                Some(*generation)
            }
            Self::Prepared(prepared) => Some(prepared.media_generation),
            Self::Cleanup {
                generation,
                remaining,
            } => remaining.capture().then_some(*generation),
        }
    }

    fn backend_generation(&self) -> Option<NonZeroU64> {
        match self {
            Self::BackendPending(generation) => Some(*generation),
            Self::Cleanup {
                generation,
                remaining,
            } => remaining.backend().then_some(*generation),
            _ => None,
        }
    }

    fn begin_cleanup(&mut self) {
        let previous = std::mem::replace(self, Self::Idle);
        *self = match previous {
            Self::Idle => Self::Idle,
            Self::StartingCapture(generation) => Self::Cleanup {
                generation,
                remaining: CleanupRemaining::Capture,
            },
            Self::Prepared(prepared) => Self::Cleanup {
                generation: prepared.media_generation,
                remaining: CleanupRemaining::Capture,
            },
            Self::BackendPending(generation) => Self::Cleanup {
                generation,
                remaining: CleanupRemaining::Both,
            },
            cleanup @ Self::Cleanup { .. } => cleanup,
        };
    }

    fn complete_capture_cleanup(&mut self) {
        if let Self::Cleanup {
            generation,
            remaining,
        } = *self
        {
            *self = remaining
                .without_capture()
                .map_or(Self::Idle, |remaining| Self::Cleanup {
                    generation,
                    remaining,
                });
        }
    }

    fn complete_backend_cleanup(&mut self) {
        if let Self::Cleanup {
            generation,
            remaining,
        } = *self
        {
            *self = remaining
                .without_backend()
                .map_or(Self::Idle, |remaining| Self::Cleanup {
                    generation,
                    remaining,
                });
        }
    }
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
            lifecycle: DriverLifecycle::Live(DriverMediaPhase::Idle),
        }
    }

    fn ensure_live(&self) -> Result<(), MediaDriverError> {
        if matches!(self.lifecycle, DriverLifecycle::Shutdown) || self.device.is_none() {
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
        let DriverLifecycle::Live(media) = &mut self.lifecycle else {
            return Err(MediaDriverError::new("media driver is shut down"));
        };
        if !matches!(media, DriverMediaPhase::Idle) {
            return Err(MediaDriverError::new(
                "a previous media generation still requires cleanup",
            ));
        }

        // Mark the generation before awaiting: cancellation or timeout can be
        // ambiguous after the capture owner observes the command.
        *media = DriverMediaPhase::StartingCapture(generation);
        let prepared = cancellable(
            cancellation.clone(),
            "start capture pipeline",
            self.capture.start(request, cancellation),
        )
        .await?;
        validate_prepared_capture(&prepared, request)?;
        *media = DriverMediaPhase::Prepared(prepared);
        Ok(())
    }

    async fn start_media(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(), MediaDriverError> {
        self.ensure_live()?;
        let generation = nonzero_generation(request.media_generation)?;
        let DriverLifecycle::Live(media) = &mut self.lifecycle else {
            return Err(MediaDriverError::new("media driver is shut down"));
        };
        if media.capture_generation() != Some(generation) {
            return Err(generation_mismatch(
                "start backend media",
                media.capture_generation(),
                generation,
            ));
        }
        let DriverMediaPhase::Prepared(prepared) = media else {
            return Err(MediaDriverError::new(
                "capture did not return media targets",
            ));
        };
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

        let prepared = match std::mem::replace(media, DriverMediaPhase::BackendPending(generation))
        {
            DriverMediaPhase::Prepared(prepared) => prepared,
            other => {
                *media = other;
                return Err(MediaDriverError::new(
                    "capture did not return media targets",
                ));
            }
        };
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
        let DriverLifecycle::Live(media) = &self.lifecycle else {
            return Err(MediaDriverError::new("media driver is shut down"));
        };
        if media.backend_generation() != Some(generation)
            || media.capture_generation() != Some(generation)
        {
            return Err(generation_mismatch(
                "suspend media",
                media.backend_generation().or(media.capture_generation()),
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
        let DriverLifecycle::Live(media) = &mut self.lifecycle else {
            return Ok(());
        };
        // Record every cleanup obligation before awaiting a port. A cancelled
        // stop leaves the remaining work available for a later attempt.
        media.begin_cleanup();
        let mut failures = Vec::new();

        if let Some(active) = media.backend_generation() {
            if active != generation {
                failures.push(
                    generation_mismatch("stop backend media", Some(active), generation).to_string(),
                );
            } else if let Some(device) = self.device.as_mut() {
                let result = cancellable(
                    cancellation.clone(),
                    "stop backend media",
                    device.stop_media(generation, map_stop_reason(reason)),
                )
                .await;
                match result {
                    Ok(()) => media.complete_backend_cleanup(),
                    Err(error) => failures.push(error.to_string()),
                }
            } else {
                // The final Device owner may have been moved into a cancelled
                // shutdown. Capture still owns an independent cleanup path.
                failures.push("Device session is no longer available".into());
            }
        }

        if let Some(active) = media.capture_generation() {
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
                    Ok(()) => media.complete_capture_cleanup(),
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
        if matches!(self.lifecycle, DriverLifecycle::Shutdown) {
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
        self.lifecycle = DriverLifecycle::Shutdown;
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
            render_device: None,
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
        let cancellation = CancellationToken::new();
        driver
            .start_capture(request(1), cancellation.clone())
            .await
            .unwrap();
        driver
            .start_media(request(1), cancellation.clone())
            .await
            .unwrap();

        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            driver.shutdown(MediaStopReason::BackendShutdown, cancellation.clone()),
        )
        .await
        .is_err());
        let error = driver
            .stop(1, MediaStopReason::TransportFailure, cancellation.clone())
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Device session is no longer available"));
        assert!(matches!(
            driver.lifecycle,
            DriverLifecycle::Live(DriverMediaPhase::Cleanup {
                generation,
                remaining: CleanupRemaining::Backend,
            }) if generation.get() == 1
        ));
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
