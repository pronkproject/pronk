use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiacast::{AppAvailability, CastApp, CastConnection, SetupInfoOutcome, APP_MIRRORING};
use pronk_backend_protocol::{
    AudioProfile, ControlKind, ControlOperation, DeviceCapabilities, DisplayIdentity, DisplayMode,
    IdentitySource, MediaConfiguration, PipeWireTarget, PreparationRequest, SessionStatistics,
    StopReason, SuspendReason, Validate, VideoProfile, MAX_ERROR_TEXT_BYTES,
    MAX_MANUFACTURER_NAME_BYTES, MAX_PRODUCT_NAME_BYTES, SESSION_FEATURE_AUDIO,
    SESSION_FEATURE_CONTROL,
};
use pronk_media::{EncodedAudioPacket, EncodedVideoAccessUnit, OPUS_SAMPLE_RATE};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use zbus::zvariant::OwnedFd;

use crate::discovery::DeviceRecord;
use crate::media::{ChromiacastMediaSession, MediaSessionError, MediaSessionEvent};
use crate::transport::{
    AudioSendOutcome, AudioSenderPort, NegotiatedVideoTransport, VideoSendOutcome, VideoSenderPort,
    VideoTransportConfiguration, VideoTransportError, VideoTransportFeedbackSnapshot,
    VideoTransportNegotiator, VideoTransportPressure,
};

const COMMAND_QUEUE_CAPACITY: usize = 8;
const EVENT_QUEUE_CAPACITY: usize = 8;
const MAX_CONNECTION_ATTEMPTS: usize = 4;
const ENDPOINT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlDeviceInfo {
    device_id: String,
    device_model: Option<String>,
    capabilities: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlSetupInfo {
    Available {
        manufacturer: Option<String>,
        product_name: Option<String>,
        ssdp_udn: Option<String>,
    },
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MirroringAvailability {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Error)]
pub(crate) enum DeviceControlError {
    #[error("Cast control connection failed: {0}")]
    Connect(String),
    #[error("authenticated device-info query failed: {0}")]
    DeviceInfo(String),
    #[error("setup-endpoint product-info query failed: {0}")]
    SetupInfo(String),
    #[error("mirroring availability query failed: {0}")]
    MirroringAvailability(String),
    #[error("Device control operation is not supported: {0}")]
    UnsupportedControl(String),
    #[error("Cast receiver control failed: {0}")]
    Control(String),
    #[error("Cast control shutdown failed: {0}")]
    Close(String),
}

#[async_trait]
pub(crate) trait DeviceControl:
    Debug + Send + Sync + VideoTransportNegotiator + 'static
{
    async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError>;
    async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError>;
    async fn get_mirroring_availability(&self)
        -> Result<MirroringAvailability, DeviceControlError>;
    async fn transmit_control(
        &mut self,
        _operation: &ControlOperation,
    ) -> Result<(), DeviceControlError> {
        Err(DeviceControlError::UnsupportedControl(
            "test or alternate Device control has no control implementation".into(),
        ))
    }
    async fn close(self: Box<Self>) -> Result<(), DeviceControlError>;
}

#[async_trait]
pub(crate) trait DeviceConnector: Debug + Send + Sync + 'static {
    async fn connect(
        &self,
        endpoint: SocketAddr,
    ) -> Result<Box<dyn DeviceControl>, DeviceControlError>;
}

#[derive(Debug, Default)]
pub(crate) struct ChromiacastDeviceConnector;

#[async_trait]
impl DeviceConnector for ChromiacastDeviceConnector {
    async fn connect(
        &self,
        endpoint: SocketAddr,
    ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
        let connection = CastConnection::connect_address(endpoint)
            .await
            .map_err(|error| DeviceControlError::Connect(error.to_string()))?;
        Ok(Box::new(ChromiacastDeviceControl {
            connection,
            active_app: None,
        }))
    }
}

struct ChromiacastDeviceControl {
    connection: CastConnection,
    active_app: Option<CastApp>,
}

impl Debug for ChromiacastDeviceControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChromiacastDeviceControl")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl VideoTransportNegotiator for ChromiacastDeviceControl {
    async fn negotiate_video(
        &mut self,
        configuration: VideoTransportConfiguration,
    ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
        if self.active_app.is_some() {
            return Err(VideoTransportError::new(
                "a Cast mirroring application is already active",
            ));
        }
        let (app, sender) =
            crate::cast_transport::negotiate_video(&self.connection, configuration).await?;
        self.active_app = Some(app);
        Ok(sender)
    }

    async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
        let Some(app) = self.active_app.as_ref() else {
            return Ok(());
        };
        self.connection.stop(app).await.map_err(|error| {
            VideoTransportError::new(format!("stop Cast mirroring app: {error}"))
        })?;
        self.active_app = None;
        Ok(())
    }
}

#[async_trait]
impl DeviceControl for ChromiacastDeviceControl {
    async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError> {
        let info = self
            .connection
            .get_device_info()
            .await
            .map_err(|error| DeviceControlError::DeviceInfo(error.to_string()))?;
        Ok(ControlDeviceInfo {
            device_id: info.device_id().into(),
            device_model: info.device_model().map(str::to_owned),
            capabilities: info.capabilities(),
        })
    }

    async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError> {
        match self
            .connection
            .get_setup_device_info()
            .await
            .map_err(|error| DeviceControlError::SetupInfo(error.to_string()))?
        {
            SetupInfoOutcome::Available(info) => Ok(ControlSetupInfo::Available {
                manufacturer: info.manufacturer().map(str::to_owned),
                product_name: info.product_name().map(str::to_owned),
                ssdp_udn: info.ssdp_udn().map(str::to_owned),
            }),
            SetupInfoOutcome::Unsupported => Ok(ControlSetupInfo::Unsupported),
            _ => Err(DeviceControlError::SetupInfo(
                "unsupported setup-info outcome".into(),
            )),
        }
    }

    async fn get_mirroring_availability(
        &self,
    ) -> Result<MirroringAvailability, DeviceControlError> {
        match self
            .connection
            .get_app_availability(APP_MIRRORING)
            .await
            .map_err(|error| DeviceControlError::MirroringAvailability(error.to_string()))?
        {
            AppAvailability::Available => Ok(MirroringAvailability::Available),
            AppAvailability::Unavailable => Ok(MirroringAvailability::Unavailable),
            _ => Err(DeviceControlError::MirroringAvailability(
                "unsupported application-availability outcome".into(),
            )),
        }
    }

    async fn transmit_control(
        &mut self,
        operation: &ControlOperation,
    ) -> Result<(), DeviceControlError> {
        match operation.kind {
            ControlKind::Volume => {
                let level = match operation.code.as_deref() {
                    Some("absolute") => f64::from(operation.value) / 100.0,
                    Some("relative") => {
                        let current = self
                            .connection
                            .status()
                            .await
                            .map_err(|error| DeviceControlError::Control(error.to_string()))?
                            .volume_level()
                            .ok_or_else(|| {
                                DeviceControlError::Control(
                                    "receiver status omitted its volume level".into(),
                                )
                            })?;
                        (current + f64::from(operation.value) / 100.0).clamp(0.0, 1.0)
                    }
                    _ => {
                        return Err(DeviceControlError::UnsupportedControl(
                            "unknown volume operation".into(),
                        ))
                    }
                };
                self.connection
                    .set_volume_level(level)
                    .await
                    .map_err(|error| DeviceControlError::Control(error.to_string()))?;
                Ok(())
            }
            ControlKind::Mute => {
                let muted = match operation.code.as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    Some("toggle") => !self
                        .connection
                        .status()
                        .await
                        .map_err(|error| DeviceControlError::Control(error.to_string()))?
                        .is_muted()
                        .ok_or_else(|| {
                            DeviceControlError::Control(
                                "receiver status omitted its mute state".into(),
                            )
                        })?,
                    _ => {
                        return Err(DeviceControlError::UnsupportedControl(
                            "unknown mute operation".into(),
                        ))
                    }
                };
                self.connection
                    .set_muted(muted)
                    .await
                    .map_err(|error| DeviceControlError::Control(error.to_string()))?;
                Ok(())
            }
            kind => Err(DeviceControlError::UnsupportedControl(format!(
                "{kind:?} has no proven Cast receiver operation"
            ))),
        }
    }

    async fn close(mut self: Box<Self>) -> Result<(), DeviceControlError> {
        let stop_result = self.stop_video().await;
        let close_result = self
            .connection
            .close()
            .await
            .map_err(|error| DeviceControlError::Close(error.to_string()));
        combine_control_close_results(stop_result, close_result)
    }
}

fn combine_control_close_results(
    stop_result: Result<(), VideoTransportError>,
    close_result: Result<(), DeviceControlError>,
) -> Result<(), DeviceControlError> {
    match (stop_result, close_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(stop_error), Ok(())) => Err(DeviceControlError::Close(stop_error.to_string())),
        (Ok(()), Err(close_error)) => Err(close_error),
        (Err(stop_error), Err(close_error)) => Err(DeviceControlError::Close(format!(
            "{stop_error}; connection close also failed: {close_error}"
        ))),
    }
}

#[derive(Debug, Default)]
pub(crate) struct FixtureDeviceConnector;

#[async_trait]
impl DeviceConnector for FixtureDeviceConnector {
    async fn connect(
        &self,
        _endpoint: SocketAddr,
    ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
        Ok(Box::new(FixtureDeviceControl))
    }
}

#[derive(Debug)]
struct FixtureDeviceControl;

#[derive(Debug)]
struct FixtureVideoSender {
    feedback: watch::Sender<VideoTransportFeedbackSnapshot>,
    feedback_sent: bool,
}

#[derive(Debug)]
struct FixtureAudioSender {
    feedback: watch::Sender<VideoTransportFeedbackSnapshot>,
}

#[async_trait]
impl AudioSenderPort for FixtureAudioSender {
    async fn send(
        &mut self,
        _packet: EncodedAudioPacket,
    ) -> Result<AudioSendOutcome, VideoTransportError> {
        self.feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_audio_packets =
                snapshot.acknowledged_audio_packets.saturating_add(1);
        });
        Ok(AudioSendOutcome::Accepted)
    }

    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

#[async_trait]
impl VideoSenderPort for FixtureVideoSender {
    async fn send(
        &mut self,
        _access_unit: EncodedVideoAccessUnit,
    ) -> Result<VideoSendOutcome, VideoTransportError> {
        let emit_initial_pressure = !self.feedback_sent;
        self.feedback_sent = true;
        self.feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_frames = snapshot.acknowledged_frames.saturating_add(1);
            if emit_initial_pressure {
                snapshot.key_frame_requests = snapshot.key_frame_requests.saturating_add(1);
                snapshot.pressure = Some(VideoTransportPressure {
                    in_flight_frames: 12,
                    in_flight_media_duration: Duration::from_millis(250),
                    max_acceptable_in_flight_duration: Duration::from_millis(100),
                    current_rtt: Some(Duration::from_millis(80)),
                    frames_dropped_or_skipped: 1,
                    fraction_lost: Some(32),
                });
            }
        });
        if emit_initial_pressure {
            let feedback = self.feedback.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                feedback.send_modify(|snapshot| {
                    snapshot.revision = snapshot.revision.saturating_add(1);
                    snapshot.pressure = Some(VideoTransportPressure {
                        max_acceptable_in_flight_duration: Duration::from_millis(100),
                        ..VideoTransportPressure::default()
                    });
                });
            });
        }
        Ok(VideoSendOutcome::Accepted)
    }

    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

#[async_trait]
impl VideoTransportNegotiator for FixtureDeviceControl {
    async fn negotiate_video(
        &mut self,
        configuration: VideoTransportConfiguration,
    ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
        Ok(fixture_video_transport(configuration.audio.is_some()))
    }

    async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

fn fixture_video_transport(with_audio: bool) -> NegotiatedVideoTransport {
    let (feedback, receiver) = watch::channel(VideoTransportFeedbackSnapshot::default());
    NegotiatedVideoTransport {
        sender: Box::new(FixtureVideoSender {
            feedback: feedback.clone(),
            feedback_sent: false,
        }),
        audio_sender: with_audio
            .then(|| Box::new(FixtureAudioSender { feedback }) as Box<dyn AudioSenderPort>),
        feedback: receiver,
        minimum_bitrate: std::num::NonZeroU32::new(500_000),
    }
}

#[async_trait]
impl DeviceControl for FixtureDeviceControl {
    async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError> {
        Ok(ControlDeviceInfo {
            device_id: "00112233-4455-6677-8899-AABBCCDDEEFF".into(),
            device_model: Some("Authenticated Device Model".into()),
            capabilities: Some(5),
        })
    }

    async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError> {
        Ok(ControlSetupInfo::Available {
            manufacturer: Some("Sony Corporation".into()),
            product_name: Some("BRAVIA 8".into()),
            ssdp_udn: Some("uuid:00112233-4455-6677-8899-aabbccddeeff".into()),
        })
    }

    async fn get_mirroring_availability(
        &self,
    ) -> Result<MirroringAvailability, DeviceControlError> {
        Ok(MirroringAvailability::Available)
    }

    async fn transmit_control(
        &mut self,
        _operation: &ControlOperation,
    ) -> Result<(), DeviceControlError> {
        Ok(())
    }

    async fn close(self: Box<Self>) -> Result<(), DeviceControlError> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DeviceActorHandle {
    commands: mpsc::Sender<DeviceCommand>,
}

impl DeviceActorHandle {
    pub(crate) async fn prepare(
        &self,
        request: PreparationRequest,
    ) -> Result<DeviceCapabilities, DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::Prepare {
                request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn configure_media(
        &self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: u64,
    ) -> Result<(), DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::ConfigureMedia {
                remotes,
                targets,
                configuration,
                media_generation,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn start_media(&self, media_generation: u64) -> Result<(), DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::StartMedia {
                media_generation,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn suspend_media(
        &self,
        reason: SuspendReason,
    ) -> Result<(), DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::SuspendMedia {
                reason,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn resume_media(&self, media_generation: u64) -> Result<(), DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::ResumeMedia {
                media_generation,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn stop_media(
        &self,
        media_generation: u64,
        reason: StopReason,
    ) -> Result<(), DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::StopMedia {
                media_generation,
                reason,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn transmit_control(
        &self,
        operation: ControlOperation,
    ) -> Result<u64, DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::TransmitControl {
                operation,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    pub(crate) async fn statistics(&self) -> Result<SessionStatistics, DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::Statistics { reply: reply_tx })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }

    async fn shutdown(&self) -> Result<(), DeviceActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(DeviceCommand::Shutdown { reply: reply_tx })
            .await
            .map_err(|_| DeviceActorError::Stopped)?;
        reply_rx.await.map_err(|_| DeviceActorError::Stopped)?
    }
}

#[derive(Debug)]
pub(crate) struct DeviceActor {
    handle: DeviceActorHandle,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceEvent {
    KeyFrameRequested {
        session_generation: u64,
        media_generation: u64,
    },
    ControlCompleted {
        session_generation: u64,
        operation_id: u64,
        succeeded: bool,
        error_text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceBitrateRequest {
    pub(crate) session_generation: u64,
    pub(crate) media_generation: u64,
    pub(crate) bitrate: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceFatalError {
    pub(crate) session_generation: u64,
    pub(crate) error_text: String,
}

#[derive(Debug)]
pub(crate) struct DeviceEventReceivers {
    pub(crate) events: mpsc::Receiver<DeviceEvent>,
    pub(crate) bitrate_requests: watch::Receiver<Option<DeviceBitrateRequest>>,
    pub(crate) fatal_error: oneshot::Receiver<DeviceFatalError>,
}

#[derive(Debug)]
struct DeviceEventSink {
    events: mpsc::Sender<DeviceEvent>,
    bitrate_requests: watch::Sender<Option<DeviceBitrateRequest>>,
    fatal_error: Option<oneshot::Sender<DeviceFatalError>>,
}

impl DeviceEventSink {
    fn send_fatal_error(&mut self, error: DeviceFatalError) {
        if let Some(sender) = self.fatal_error.take() {
            let _ = sender.send(error);
        }
    }
}

impl DeviceActor {
    pub(crate) fn spawn(
        device: DeviceRecord,
        session_id: String,
        session_generation: u64,
        allowed_features: u64,
        connector: Arc<dyn DeviceConnector>,
    ) -> Result<(Self, DeviceActorHandle, DeviceEventReceivers), DeviceActorError> {
        let media = ChromiacastMediaSession::spawn(session_id, session_generation)?;
        let feedback = media.subscribe_feedback()?;
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let (bitrate_request_tx, bitrate_request_rx) = watch::channel(None);
        let (fatal_error_tx, fatal_error_rx) = oneshot::channel();
        let handle = DeviceActorHandle {
            commands: command_tx,
        };
        let task = tokio::spawn(run_actor(
            device,
            allowed_features,
            connector,
            media,
            command_rx,
            feedback,
            DeviceEventSink {
                events: event_tx,
                bitrate_requests: bitrate_request_tx,
                fatal_error: Some(fatal_error_tx),
            },
        ));
        Ok((
            Self {
                handle: handle.clone(),
                task: Some(task),
            },
            handle,
            DeviceEventReceivers {
                events: event_rx,
                bitrate_requests: bitrate_request_rx,
                fatal_error: fatal_error_rx,
            },
        ))
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), DeviceActorError> {
        let response = self.handle.shutdown().await;
        if let Some(task) = self.task.take() {
            task.await.map_err(|_| DeviceActorError::Stopped)?;
        }
        match response {
            Err(DeviceActorError::Stopped) => Ok(()),
            response => response,
        }
    }
}

impl Drop for DeviceActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum DeviceActorError {
    #[error("device actor has stopped")]
    Stopped,
    #[error("preparation request is invalid: {0}")]
    InvalidRequest(String),
    #[error("Prepare is callable exactly once after success")]
    AlreadyPrepared,
    #[error("selected device has no authenticated reachable endpoint")]
    ConnectFailed,
    #[error("{0}")]
    DeviceInfoFailed(String),
    #[error("{0}")]
    MirroringAvailabilityFailed(String),
    #[error("the selected device identity changed during authentication")]
    DeviceIdentityChanged,
    #[error("the selected device does not support screen mirroring")]
    MirroringUnavailable,
    #[error("authenticated receiver does not advertise video output")]
    VideoUnavailable,
    #[error("selected display identity is invalid: {0}")]
    InvalidIdentity(String),
    #[error("the preparation offer has no supported video mode")]
    NoSupportedMode,
    #[error("the preparation offer has no supported video profile")]
    NoSupportedVideoProfile,
    #[error("the preparation offer has no supported Opus audio profile")]
    NoSupportedAudioProfile,
    #[error("Cast control shutdown failed: {0}")]
    CloseFailed(String),
    #[error("multiple device shutdown operations failed: {0}")]
    ShutdownFailed(String),
    #[error(transparent)]
    Media(#[from] MediaSessionError),
}

#[derive(Debug)]
enum DeviceCommand {
    Prepare {
        request: PreparationRequest,
        reply: oneshot::Sender<Result<DeviceCapabilities, DeviceActorError>>,
    },
    ConfigureMedia {
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: u64,
        reply: oneshot::Sender<Result<(), DeviceActorError>>,
    },
    StartMedia {
        media_generation: u64,
        reply: oneshot::Sender<Result<(), DeviceActorError>>,
    },
    SuspendMedia {
        reason: SuspendReason,
        reply: oneshot::Sender<Result<(), DeviceActorError>>,
    },
    ResumeMedia {
        media_generation: u64,
        reply: oneshot::Sender<Result<(), DeviceActorError>>,
    },
    StopMedia {
        media_generation: u64,
        reason: StopReason,
        reply: oneshot::Sender<Result<(), DeviceActorError>>,
    },
    TransmitControl {
        operation: ControlOperation,
        reply: oneshot::Sender<Result<u64, DeviceActorError>>,
    },
    Statistics {
        reply: oneshot::Sender<Result<SessionStatistics, DeviceActorError>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), DeviceActorError>>,
    },
}

enum NextDeviceInput {
    Command(Option<DeviceCommand>),
    Feedback(Result<(), watch::error::RecvError>),
}

async fn run_actor(
    device: DeviceRecord,
    allowed_features: u64,
    connector: Arc<dyn DeviceConnector>,
    mut media: ChromiacastMediaSession,
    mut commands: mpsc::Receiver<DeviceCommand>,
    mut feedback: watch::Receiver<crate::sender_actor::VideoSenderFeedbackSnapshot>,
    mut events: DeviceEventSink,
) {
    let mut control = None;
    let mut next_control_operation = 1_u64;
    let mut feedback_open = true;
    loop {
        let next = if feedback_open {
            tokio::select! {
                biased;
                command = commands.recv() => NextDeviceInput::Command(command),
                changed = feedback.changed() => NextDeviceInput::Feedback(changed),
            }
        } else {
            NextDeviceInput::Command(commands.recv().await)
        };
        let command = match next {
            NextDeviceInput::Command(command) => command,
            NextDeviceInput::Feedback(Ok(())) => {
                let feedback = feedback.borrow_and_update().clone();
                match media.handle_feedback(feedback).await {
                    Ok(media_events) => forward_media_events(&events, media_events),
                    Err(error) => {
                        events.send_fatal_error(DeviceFatalError {
                            session_generation: media.session_generation(),
                            error_text: error.to_string(),
                        });
                        commands.close();
                        break;
                    }
                }
                continue;
            }
            NextDeviceInput::Feedback(Err(_)) => {
                feedback_open = false;
                continue;
            }
        };
        let Some(command) = command else {
            break;
        };
        match command {
            DeviceCommand::Prepare { request, reply } => {
                let result = prepare_device(
                    &device,
                    allowed_features,
                    connector.as_ref(),
                    &mut control,
                    &mut media,
                    request,
                )
                .await;
                let _ = reply.send(result);
            }
            DeviceCommand::ConfigureMedia {
                remotes,
                targets,
                configuration,
                media_generation,
                reply,
            } => {
                let result = match control.as_deref_mut() {
                    Some(control) => media
                        .configure(remotes, targets, configuration, media_generation, control)
                        .await
                        .map_err(DeviceActorError::from),
                    None => Err(DeviceActorError::InvalidRequest(
                        "ConfigureMedia requires a prepared device connection".into(),
                    )),
                };
                let _ = reply.send(result);
            }
            DeviceCommand::StartMedia {
                media_generation,
                reply,
            } => {
                let result = media
                    .start(media_generation)
                    .await
                    .map_err(DeviceActorError::from);
                let _ = reply.send(result);
            }
            DeviceCommand::SuspendMedia { reason, reply } => {
                let _ = reason;
                let result = media.suspend().await.map_err(DeviceActorError::from);
                let _ = reply.send(result);
            }
            DeviceCommand::ResumeMedia {
                media_generation,
                reply,
            } => {
                let result = media
                    .resume(media_generation)
                    .await
                    .map_err(DeviceActorError::from);
                let _ = reply.send(result);
            }
            DeviceCommand::StopMedia {
                media_generation,
                reason,
                reply,
            } => {
                let _ = reason;
                let result = match control.as_deref_mut() {
                    Some(control) => media
                        .stop_media(media_generation, control)
                        .await
                        .map_err(DeviceActorError::from),
                    None => Err(DeviceActorError::InvalidRequest(
                        "StopMedia requires a prepared device connection".into(),
                    )),
                };
                let _ = reply.send(result);
            }
            DeviceCommand::TransmitControl { operation, reply } => {
                let result = operation
                    .validate()
                    .map_err(|error| DeviceActorError::InvalidRequest(error.to_string()))
                    .and_then(|()| {
                        if allowed_features & SESSION_FEATURE_CONTROL == 0 {
                            return Err(DeviceActorError::InvalidRequest(
                                "control was not requested for this Device session".into(),
                            ));
                        }
                        if operation.session_generation != media.session_generation() {
                            return Err(DeviceActorError::InvalidRequest(format!(
                                "control session generation {} differs from {}",
                                operation.session_generation,
                                media.session_generation()
                            )));
                        }
                        if control.is_none() {
                            return Err(DeviceActorError::InvalidRequest(
                                "TransmitControl requires a prepared Device connection".into(),
                            ));
                        }
                        let operation_id = next_control_operation;
                        next_control_operation =
                            next_control_operation.checked_add(1).ok_or_else(|| {
                                DeviceActorError::InvalidRequest(
                                    "control operation IDs are exhausted".into(),
                                )
                            })?;
                        Ok(operation_id)
                    });
                let operation_id = match result {
                    Ok(operation_id) => operation_id,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                if reply.send(Ok(operation_id)).is_err() {
                    continue;
                }
                let result = control
                    .as_deref_mut()
                    .expect("validated control operation has a prepared connection")
                    .transmit_control(&operation)
                    .await;
                let (succeeded, error_text) = match result {
                    Ok(()) => (true, String::new()),
                    Err(error) => (false, bounded_control_error(&error)),
                };
                let _ = events
                    .events
                    .send(DeviceEvent::ControlCompleted {
                        session_generation: media.session_generation(),
                        operation_id,
                        succeeded,
                        error_text,
                    })
                    .await;
            }
            DeviceCommand::Statistics { reply } => {
                let result = media.statistics().await.map_err(DeviceActorError::from);
                let _ = reply.send(result);
            }
            DeviceCommand::Shutdown { reply } => {
                let result = shutdown_device(&mut media, &mut control).await;
                let _ = reply.send(result);
                return;
            }
        }
    }
    let _ = shutdown_device(&mut media, &mut control).await;
}

fn forward_media_events(events: &DeviceEventSink, media_events: Vec<MediaSessionEvent>) {
    for event in media_events {
        let event = match event {
            MediaSessionEvent::KeyFrameRequested {
                session_generation,
                media_generation,
            } => DeviceEvent::KeyFrameRequested {
                session_generation,
                media_generation,
            },
            MediaSessionEvent::BitrateRequested {
                session_generation,
                media_generation,
                bitrate,
            } => {
                events
                    .bitrate_requests
                    .send_replace(Some(DeviceBitrateRequest {
                        session_generation,
                        media_generation,
                        bitrate,
                    }));
                continue;
            }
        };
        let _ = events.events.try_send(event);
    }
}

fn bounded_control_error(error: &DeviceControlError) -> String {
    let filtered: String = error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let filtered = filtered.trim();
    let filtered = if filtered.is_empty() {
        "Device control operation failed"
    } else {
        filtered
    };
    if filtered.len() <= MAX_ERROR_TEXT_BYTES {
        return filtered.into();
    }
    let mut end = MAX_ERROR_TEXT_BYTES;
    while !filtered.is_char_boundary(end) {
        end -= 1;
    }
    filtered[..end].trim_end().into()
}

async fn prepare_device(
    device: &DeviceRecord,
    allowed_features: u64,
    connector: &dyn DeviceConnector,
    control_slot: &mut Option<Box<dyn DeviceControl>>,
    media: &mut ChromiacastMediaSession,
    request: PreparationRequest,
) -> Result<DeviceCapabilities, DeviceActorError> {
    request
        .validate()
        .map_err(|error| DeviceActorError::InvalidRequest(error.to_string()))?;
    if request.requested_features & !allowed_features != 0 {
        return Err(DeviceActorError::InvalidRequest(
            "preparation requests features absent from SessionOptions".into(),
        ));
    }
    if media.is_prepared() {
        return Err(DeviceActorError::AlreadyPrepared);
    }

    let control = connect(device, connector).await?;
    let query = query_identity(device, control.as_ref()).await;
    let identity = match query {
        Ok(identity) => identity,
        Err(error) => {
            let _ = control.close().await;
            return Err(error);
        }
    };
    let capabilities = match negotiate_capabilities(request, identity) {
        Ok(capabilities) => capabilities,
        Err(error) => {
            let _ = control.close().await;
            return Err(error);
        }
    };
    if let Err(error) = media.complete_preparation(capabilities.clone()) {
        let _ = control.close().await;
        return Err(error.into());
    }
    *control_slot = Some(control);
    Ok(capabilities)
}

async fn connect(
    device: &DeviceRecord,
    connector: &dyn DeviceConnector,
) -> Result<Box<dyn DeviceControl>, DeviceActorError> {
    for endpoint in device.endpoints.iter().take(MAX_CONNECTION_ATTEMPTS) {
        match tokio::time::timeout(ENDPOINT_ATTEMPT_TIMEOUT, connector.connect(*endpoint)).await {
            Ok(Ok(control)) => return Ok(control),
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    Err(DeviceActorError::ConnectFailed)
}

async fn query_identity(
    selected: &DeviceRecord,
    control: &dyn DeviceControl,
) -> Result<DisplayIdentity, DeviceActorError> {
    let (device_info, setup_info, mirroring) = tokio::join!(
        control.get_device_info(),
        control.get_setup_info(),
        control.get_mirroring_availability(),
    );
    let device_info =
        device_info.map_err(|error| DeviceActorError::DeviceInfoFailed(error.to_string()))?;
    let setup_info = match setup_info {
        Ok(info) => usable_setup_info(selected, info),
        Err(error) => {
            tracing::debug!(%error, "optional Cast setup metadata is unavailable");
            None
        }
    };
    let mirroring = mirroring
        .map_err(|error| DeviceActorError::MirroringAvailabilityFailed(error.to_string()))?;

    if normalize_cast_device_id(&selected.info.device_id)?
        != normalize_cast_device_id(&device_info.device_id)?
    {
        return Err(DeviceActorError::DeviceIdentityChanged);
    }
    if device_info
        .capabilities
        .is_some_and(|capabilities| capabilities & 1 == 0)
    {
        return Err(DeviceActorError::VideoUnavailable);
    }
    if mirroring != MirroringAvailability::Available {
        return Err(DeviceActorError::MirroringUnavailable);
    }

    let (manufacturer, product, product_source) = match setup_info {
        Some(ControlSetupIdentity {
            manufacturer,
            product_name,
        }) => {
            let (product, source) = match product_name {
                Some(product) => (Some(product), IdentitySource::SetupEndpoint),
                None => {
                    let product = device_info
                        .device_model
                        .map(|model| {
                            bounded_identity("device model", model, MAX_PRODUCT_NAME_BYTES)
                        })
                        .transpose()?;
                    let source = if product.is_some() {
                        IdentitySource::AuthenticatedDeviceInfo
                    } else {
                        IdentitySource::Absent
                    };
                    (product, source)
                }
            };
            (manufacturer, product, source)
        }
        None => {
            let product = device_info
                .device_model
                .map(|model| bounded_identity("device model", model, MAX_PRODUCT_NAME_BYTES))
                .transpose()?;
            let source = if product.is_some() {
                IdentitySource::AuthenticatedDeviceInfo
            } else {
                IdentitySource::Absent
            };
            (None, product, source)
        }
    };
    let identity = DisplayIdentity {
        manufacturer_source: if manufacturer.is_some() {
            IdentitySource::SetupEndpoint
        } else {
            IdentitySource::Absent
        },
        manufacturer_name: manufacturer,
        product_name: product,
        product_source,
        pnp_id: None,
    };
    identity
        .validate()
        .map_err(|error| DeviceActorError::InvalidIdentity(error.to_string()))?;
    Ok(identity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlSetupIdentity {
    manufacturer: Option<String>,
    product_name: Option<String>,
}

fn usable_setup_info(
    selected: &DeviceRecord,
    setup: ControlSetupInfo,
) -> Option<ControlSetupIdentity> {
    let ControlSetupInfo::Available {
        manufacturer,
        product_name,
        ssdp_udn,
    } = setup
    else {
        return None;
    };
    let validated = (|| {
        if let Some(ssdp_udn) = ssdp_udn {
            if normalize_cast_device_id(&selected.info.device_id)?
                != normalize_cast_device_id(&ssdp_udn)?
            {
                return Err(DeviceActorError::DeviceIdentityChanged);
            }
        }
        Ok(ControlSetupIdentity {
            manufacturer: manufacturer
                .map(|value| bounded_identity("manufacturer", value, MAX_MANUFACTURER_NAME_BYTES))
                .transpose()?,
            product_name: product_name
                .map(|value| bounded_identity("product name", value, MAX_PRODUCT_NAME_BYTES))
                .transpose()?,
        })
    })();
    match validated {
        Ok(info) => Some(info),
        Err(error) => {
            tracing::debug!(%error, "ignoring unusable Cast setup metadata");
            None
        }
    }
}

fn normalize_cast_device_id(value: &str) -> Result<String, DeviceActorError> {
    let value = value.trim();
    let value = value
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("uuid:"))
        .map_or(value, |_| &value[5..]);
    let normalized: String = value
        .bytes()
        .filter(|byte| *byte != b'-')
        .map(|byte| (byte as char).to_ascii_lowercase())
        .collect();
    if normalized.len() != 32 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DeviceActorError::InvalidIdentity(
            "Cast device ID is not a UUID".into(),
        ));
    }
    Ok(normalized)
}

fn bounded_identity(
    field: &'static str,
    value: String,
    maximum: usize,
) -> Result<String, DeviceActorError> {
    let value = value.trim();
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(DeviceActorError::InvalidIdentity(format!(
            "{field} is empty, too long, or contains a control character"
        )));
    }
    Ok(value.into())
}

fn negotiate_capabilities(
    request: PreparationRequest,
    display_identity: DisplayIdentity,
) -> Result<DeviceCapabilities, DeviceActorError> {
    let audio_requested = request.requested_features & SESSION_FEATURE_AUDIO != 0;
    let control_requested = request.requested_features & SESSION_FEATURE_CONTROL != 0;
    let modes: Vec<_> = request
        .candidate_modes
        .into_iter()
        .filter(supported_h264_sender_mode)
        .collect();
    if modes.is_empty() {
        return Err(DeviceActorError::NoSupportedMode);
    }
    let video_profiles: Vec<_> = request
        .video_profiles
        .into_iter()
        .filter_map(narrow_h264_profile)
        .take(1)
        .collect();
    if video_profiles.is_empty() {
        return Err(DeviceActorError::NoSupportedVideoProfile);
    }
    let audio_profiles: Vec<_> = if audio_requested {
        request
            .audio_profiles
            .into_iter()
            .filter_map(narrow_opus_profile)
            .take(1)
            .collect()
    } else {
        Vec::new()
    };
    if audio_requested && audio_profiles.is_empty() {
        return Err(DeviceActorError::NoSupportedAudioProfile);
    }
    let capabilities = DeviceCapabilities {
        preparation_generation: request.preparation_generation,
        display_identity,
        modes,
        video_profiles,
        audio_profiles,
        features: (u64::from(audio_requested) * SESSION_FEATURE_AUDIO)
            | (u64::from(control_requested) * SESSION_FEATURE_CONTROL),
    };
    capabilities
        .validate()
        .map_err(|error| DeviceActorError::InvalidRequest(error.to_string()))?;
    Ok(capabilities)
}

fn narrow_opus_profile(profile: AudioProfile) -> Option<AudioProfile> {
    if profile.codec != "opus"
        || profile.max_channels < 2
        || !profile.sample_rates.contains(&OPUS_SAMPLE_RATE)
    {
        return None;
    }
    Some(AudioProfile {
        profile_id: "opus-stereo".into(),
        codec: "opus".into(),
        max_channels: 2,
        sample_rates: vec![OPUS_SAMPLE_RATE],
    })
}

fn narrow_h264_profile(mut profile: VideoProfile) -> Option<VideoProfile> {
    if profile.codec != "h264" {
        return None;
    }
    profile.max_width = profile.max_width.min(3_840);
    profile.max_height = profile.max_height.min(2_160);
    profile.max_refresh_millihz = profile.max_refresh_millihz.min(60_000);
    Some(profile)
}

fn supported_h264_sender_mode(mode: &DisplayMode) -> bool {
    if mode.flags != 0 {
        return false;
    }

    // Cast transports only the encoded picture; DRM blanking and sync flags do
    // not reach the receiver. Keep presentation modes at the receiver's 16:9
    // aspect so Cast applications cannot crop wider or taller desktops while
    // fitting the video to the television. CTA EDIDs still require the VGA
    // compatibility timing, so retain that one fallback until the media path
    // can letterbox it explicitly.
    matches!(
        (mode.width, mode.height, mode.refresh_millihz),
        (3_840, 2_160, 30_000)
            | (2_560, 1_440, 60_000)
            | (1_920, 1_080, 60_000)
            | (1_600, 900, 60_000)
            | (1_366, 768, 60_000)
            | (1_280, 720, 60_000)
            | (640, 480, 60_000)
    )
}

async fn close_control(
    control: &mut Option<Box<dyn DeviceControl>>,
) -> Result<(), DeviceActorError> {
    let Some(control) = control.take() else {
        return Ok(());
    };
    control
        .close()
        .await
        .map_err(|error| DeviceActorError::CloseFailed(error.to_string()))
}

async fn shutdown_device(
    media: &mut ChromiacastMediaSession,
    control: &mut Option<Box<dyn DeviceControl>>,
) -> Result<(), DeviceActorError> {
    // The Cast control connection and media graph are independent owners.
    // Start both final cleanups so either can complete when the other wedges.
    let (media_result, control_result) = tokio::join!(
        async { media.shutdown().await.map_err(DeviceActorError::from) },
        close_control(control),
    );
    match (media_result, control_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(media_error), Err(control_error)) => Err(DeviceActorError::ShutdownFailed(format!(
            "{media_error}; {control_error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use pronk_backend_protocol::{DeviceAvailability, DeviceInfo, DisplayMode, VideoProfile};
    use pronk_media::VideoFrameDependency;

    use super::*;
    use crate::discovery::FIXTURE_DEVICE_ID;

    #[derive(Debug)]
    struct ScriptedConnector {
        device_id: &'static str,
        setup: Result<ControlSetupInfo, DeviceControlError>,
    }

    #[async_trait]
    impl DeviceConnector for ScriptedConnector {
        async fn connect(
            &self,
            _endpoint: SocketAddr,
        ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
            Ok(Box::new(ScriptedControl {
                device_id: self.device_id,
                setup: self.setup.clone(),
            }))
        }
    }

    #[derive(Debug)]
    struct ScriptedControl {
        device_id: &'static str,
        setup: Result<ControlSetupInfo, DeviceControlError>,
    }

    #[derive(Debug)]
    struct CountingConnector {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DeviceConnector for CountingConnector {
        async fn connect(
            &self,
            _endpoint: SocketAddr,
        ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FixtureDeviceControl))
        }
    }

    #[async_trait]
    impl VideoTransportNegotiator for ScriptedControl {
        async fn negotiate_video(
            &mut self,
            _configuration: VideoTransportConfiguration,
        ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
            Ok(fixture_video_transport(false))
        }

        async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[async_trait]
    impl DeviceControl for ScriptedControl {
        async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError> {
            Ok(ControlDeviceInfo {
                device_id: self.device_id.into(),
                device_model: Some("Device Model".into()),
                capabilities: Some(5),
            })
        }

        async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError> {
            self.setup.clone()
        }

        async fn get_mirroring_availability(
            &self,
        ) -> Result<MirroringAvailability, DeviceControlError> {
            Ok(MirroringAvailability::Available)
        }

        async fn close(self: Box<Self>) -> Result<(), DeviceControlError> {
            Ok(())
        }
    }

    fn device() -> DeviceRecord {
        DeviceRecord {
            info: DeviceInfo {
                backend_id: "chromiacast".into(),
                device_id: FIXTURE_DEVICE_ID.into(),
                display_name: "Living Room".into(),
                availability: DeviceAvailability::Available,
                metadata: Vec::new(),
            },
            endpoints: vec!["192.0.2.1:8009".parse().unwrap()],
        }
    }

    fn request() -> PreparationRequest {
        PreparationRequest {
            preparation_generation: 9,
            candidate_modes: vec![DisplayMode {
                width: 1_920,
                height: 1_080,
                refresh_millihz: 60_000,
                flags: 0,
            }],
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 1_920,
                max_height: 1_080,
                max_refresh_millihz: 60_000,
            }],
            audio_profiles: Vec::new(),
            requested_features: 0,
        }
    }

    fn spawn_actor(connector: Arc<dyn DeviceConnector>) -> (DeviceActor, DeviceActorHandle) {
        let (actor, handle, _events) = DeviceActor::spawn(
            device(),
            "12345678-1234-1234-1234-123456789abc".into(),
            1,
            0,
            connector,
        )
        .unwrap();
        (actor, handle)
    }

    #[tokio::test]
    async fn setup_endpoint_identity_is_primary_and_bounded() {
        let connector = Arc::new(ScriptedConnector {
            device_id: "00112233-4455-6677-8899-AABBCCDDEEFF",
            setup: Ok(ControlSetupInfo::Available {
                manufacturer: Some("Sony Corporation".into()),
                product_name: Some("BRAVIA 8".into()),
                ssdp_udn: Some("uuid:00112233445566778899aabbccddeeff".into()),
            }),
        });
        let (actor, handle) = spawn_actor(connector);
        let capabilities = handle.prepare(request()).await.unwrap();
        assert_eq!(
            capabilities.display_identity.manufacturer_name.as_deref(),
            Some("Sony Corporation")
        );
        assert_eq!(
            capabilities.display_identity.product_name.as_deref(),
            Some("BRAVIA 8")
        );
        assert_eq!(
            capabilities.display_identity.product_source,
            IdentitySource::SetupEndpoint
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn selected_endpoint_is_idle_until_prepare() {
        let calls = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(CountingConnector {
            calls: Arc::clone(&calls),
        });
        let (actor, handle) = spawn_actor(connector);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        handle.prepare(request()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fixture_control_uses_monotonic_ids_and_terminal_events() {
        let (actor, handle, mut receivers) = DeviceActor::spawn(
            device(),
            "12345678-1234-1234-1234-123456789abc".into(),
            7,
            SESSION_FEATURE_CONTROL,
            Arc::new(FixtureDeviceConnector),
        )
        .unwrap();
        let mut offer = request();
        offer.requested_features = SESSION_FEATURE_CONTROL;
        let capabilities = handle.prepare(offer).await.unwrap();
        assert_eq!(capabilities.features, SESSION_FEATURE_CONTROL);

        for (expected_id, operation) in [
            (
                1,
                ControlOperation {
                    session_generation: 7,
                    kind: ControlKind::Volume,
                    code: Some("relative".into()),
                    value: 5,
                },
            ),
            (
                2,
                ControlOperation {
                    session_generation: 7,
                    kind: ControlKind::Mute,
                    code: Some("toggle".into()),
                    value: 0,
                },
            ),
        ] {
            assert_eq!(
                handle.transmit_control(operation).await.unwrap(),
                expected_id
            );
            assert_eq!(
                receivers.events.recv().await.unwrap(),
                DeviceEvent::ControlCompleted {
                    session_generation: 7,
                    operation_id: expected_id,
                    succeeded: true,
                    error_text: String::new(),
                }
            );
        }
        actor.shutdown().await.unwrap();
    }

    #[test]
    fn control_shutdown_preserves_receiver_and_connection_failures() {
        let stop_error = VideoTransportError::new("receiver stop failed");
        let close_error = DeviceControlError::Close("connection close failed".into());

        let error = combine_control_close_results(Err(stop_error), Err(close_error)).unwrap_err();

        let text = error.to_string();
        assert!(text.contains("receiver stop failed"));
        assert!(text.contains("connection close failed"));
    }

    #[tokio::test]
    async fn fatal_error_is_latched_when_the_ordinary_event_queue_is_full() {
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        for operation_id in 0..EVENT_QUEUE_CAPACITY as u64 {
            event_tx
                .try_send(DeviceEvent::ControlCompleted {
                    session_generation: 7,
                    operation_id,
                    succeeded: true,
                    error_text: String::new(),
                })
                .unwrap();
        }
        let (fatal_error_tx, fatal_error_rx) = oneshot::channel();
        let mut sink = DeviceEventSink {
            events: event_tx,
            bitrate_requests: watch::channel(None).0,
            fatal_error: Some(fatal_error_tx),
        };
        let expected = DeviceFatalError {
            session_generation: 7,
            error_text: "transport failed".into(),
        };

        sink.send_fatal_error(expected.clone());

        assert_eq!(fatal_error_rx.await.unwrap(), expected);
        assert_eq!(event_rx.len(), EVENT_QUEUE_CAPACITY);
        assert!(sink.fatal_error.is_none());
        assert!(event_rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn bitrate_requests_coalesce_to_the_latest_value() {
        let (event_tx, _event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let (bitrate_tx, mut bitrate_rx) = watch::channel(None);
        let (fatal_error_tx, _fatal_error_rx) = oneshot::channel();
        let sink = DeviceEventSink {
            events: event_tx,
            bitrate_requests: bitrate_tx,
            fatal_error: Some(fatal_error_tx),
        };

        forward_media_events(
            &sink,
            vec![
                MediaSessionEvent::BitrateRequested {
                    session_generation: 7,
                    media_generation: 11,
                    bitrate: 4_000_000,
                },
                MediaSessionEvent::BitrateRequested {
                    session_generation: 7,
                    media_generation: 11,
                    bitrate: 3_000_000,
                },
            ],
        );

        bitrate_rx.changed().await.unwrap();
        assert_eq!(
            *bitrate_rx.borrow_and_update(),
            Some(DeviceBitrateRequest {
                session_generation: 7,
                media_generation: 11,
                bitrate: 3_000_000,
            })
        );
    }

    #[tokio::test]
    async fn unsupported_setup_falls_back_to_authenticated_device_model() {
        let connector = Arc::new(ScriptedConnector {
            device_id: FIXTURE_DEVICE_ID,
            setup: Ok(ControlSetupInfo::Unsupported),
        });
        let (actor, handle) = spawn_actor(connector);
        let capabilities = handle.prepare(request()).await.unwrap();
        assert_eq!(
            capabilities.display_identity.product_name.as_deref(),
            Some("Device Model")
        );
        assert_eq!(
            capabilities.display_identity.product_source,
            IdentitySource::AuthenticatedDeviceInfo
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_identity_mismatch_fails_without_retargeting() {
        let connector = Arc::new(ScriptedConnector {
            device_id: "ffeeddccbbaa99887766554433221100",
            setup: Ok(ControlSetupInfo::Unsupported),
        });
        let (actor, handle) = spawn_actor(connector);
        assert_eq!(
            handle.prepare(request()).await,
            Err(DeviceActorError::DeviceIdentityChanged)
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mismatched_setup_identity_is_ignored() {
        let connector = Arc::new(ScriptedConnector {
            device_id: FIXTURE_DEVICE_ID,
            setup: Ok(ControlSetupInfo::Available {
                manufacturer: Some("Sony".into()),
                product_name: Some("BRAVIA 8".into()),
                ssdp_udn: Some("uuid:ffeeddcc-bbaa-9988-7766-554433221100".into()),
            }),
        });
        let (actor, handle) = spawn_actor(connector);
        let capabilities = handle.prepare(request()).await.unwrap();
        assert_eq!(capabilities.display_identity.manufacturer_name, None);
        assert_eq!(
            capabilities.display_identity.product_name.as_deref(),
            Some("Device Model")
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unusable_setup_identity_falls_back_to_device_model() {
        let connector = Arc::new(ScriptedConnector {
            device_id: FIXTURE_DEVICE_ID,
            setup: Ok(ControlSetupInfo::Available {
                manufacturer: Some("Sony".into()),
                product_name: Some("x".repeat(MAX_PRODUCT_NAME_BYTES + 1)),
                ssdp_udn: Some(FIXTURE_DEVICE_ID.into()),
            }),
        });
        let (actor, handle) = spawn_actor(connector);
        let capabilities = handle.prepare(request()).await.unwrap();
        assert_eq!(capabilities.display_identity.manufacturer_name, None);
        assert_eq!(
            capabilities.display_identity.product_name.as_deref(),
            Some("Device Model")
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn setup_transport_failure_falls_back_to_device_model() {
        let connector = Arc::new(ScriptedConnector {
            device_id: FIXTURE_DEVICE_ID,
            setup: Err(DeviceControlError::SetupInfo("offline".into())),
        });
        let (actor, handle) = spawn_actor(connector);
        let capabilities = handle.prepare(request()).await.unwrap();
        assert_eq!(
            capabilities.display_identity.product_name.as_deref(),
            Some("Device Model")
        );
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fixture_transport_acknowledges_every_accepted_stream() {
        let generation = NonZeroU64::new(1).unwrap();
        let mut transport = fixture_video_transport(true);
        let mut feedback = transport.feedback.clone();

        assert_eq!(
            transport
                .sender
                .send(EncodedVideoAccessUnit {
                    media_generation: generation,
                    dependency: VideoFrameDependency::KeyFrame,
                    data: vec![0, 0, 0, 1, 0x65],
                    media_timestamp: Duration::ZERO,
                    reference_time: Instant::now(),
                    duration: Duration::from_millis(16),
                })
                .await
                .unwrap(),
            VideoSendOutcome::Accepted
        );
        feedback.changed().await.unwrap();
        assert_eq!(feedback.borrow().acknowledged_frames, 1);

        assert_eq!(
            transport
                .audio_sender
                .as_mut()
                .unwrap()
                .send(EncodedAudioPacket {
                    media_generation: generation,
                    data: vec![0xf8, 0xff, 0xfe],
                    media_timestamp: Duration::ZERO,
                    reference_time: Instant::now(),
                    duration: Duration::from_millis(20),
                })
                .await
                .unwrap(),
            AudioSendOutcome::Accepted
        );
        feedback.changed().await.unwrap();
        assert_eq!(feedback.borrow().acknowledged_audio_packets, 1);
    }

    #[test]
    fn video_capability_keeps_safe_presentation_and_compatibility_modes() {
        let mut offer = request();
        offer.candidate_modes = vec![
            DisplayMode {
                width: 3_840,
                height: 2_160,
                refresh_millihz: 30_000,
                flags: 0,
            },
            DisplayMode {
                width: 3_840,
                height: 2_160,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 2_560,
                height: 1_440,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 1_680,
                height: 1_050,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 1_366,
                height: 768,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 640,
                height: 480,
                refresh_millihz: 60_000,
                flags: 0,
            },
        ];
        offer.video_profiles[0].max_width = 7_680;
        offer.video_profiles[0].max_height = 4_320;
        offer.video_profiles[0].max_refresh_millihz = 240_000;

        let capabilities = negotiate_capabilities(offer, display_identity()).unwrap();
        assert_eq!(capabilities.modes.len(), 4);
        assert!(capabilities.modes.iter().any(|mode| (
            mode.width,
            mode.height,
            mode.refresh_millihz
        ) == (3_840, 2_160, 30_000)));
        assert!(!capabilities.modes.iter().any(|mode| (
            mode.width,
            mode.height,
            mode.refresh_millihz
        ) == (3_840, 2_160, 60_000)));
        assert!(!capabilities.modes.iter().any(|mode| (
            mode.width,
            mode.height,
            mode.refresh_millihz
        ) == (1_680, 1_050, 60_000)));
        assert!(capabilities.modes.iter().any(|mode| (
            mode.width,
            mode.height,
            mode.refresh_millihz
        ) == (1_366, 768, 60_000)));
        assert!(capabilities.modes.iter().any(|mode| (
            mode.width,
            mode.height,
            mode.refresh_millihz
        ) == (640, 480, 60_000)));
        assert_eq!(capabilities.video_profiles[0].max_width, 3_840);
        assert_eq!(capabilities.video_profiles[0].max_height, 2_160);
        assert_eq!(capabilities.video_profiles[0].max_refresh_millihz, 60_000);
    }

    #[test]
    fn audio_capability_is_narrowed_to_the_supported_opus_contract() {
        let mut supported_request = request();
        supported_request.requested_features = SESSION_FEATURE_AUDIO;
        supported_request.audio_profiles = vec![AudioProfile {
            profile_id: "opus-flexible".into(),
            codec: "opus".into(),
            max_channels: 6,
            sample_rates: vec![44_100, OPUS_SAMPLE_RATE],
        }];
        let capabilities = negotiate_capabilities(supported_request, display_identity()).unwrap();
        assert_eq!(capabilities.features, SESSION_FEATURE_AUDIO);
        assert_eq!(
            capabilities.audio_profiles,
            [AudioProfile {
                profile_id: "opus-stereo".into(),
                codec: "opus".into(),
                max_channels: 2,
                sample_rates: vec![OPUS_SAMPLE_RATE],
            }]
        );

        let mut unsupported_request = request();
        unsupported_request.requested_features = SESSION_FEATURE_AUDIO;
        unsupported_request.audio_profiles = vec![AudioProfile {
            profile_id: "opus-44100".into(),
            codec: "opus".into(),
            max_channels: 2,
            sample_rates: vec![44_100],
        }];
        assert_eq!(
            negotiate_capabilities(unsupported_request, display_identity()),
            Err(DeviceActorError::NoSupportedAudioProfile)
        );
    }

    fn display_identity() -> DisplayIdentity {
        DisplayIdentity {
            manufacturer_name: Some("Sony".into()),
            manufacturer_source: IdentitySource::SetupEndpoint,
            product_name: Some("BRAVIA".into()),
            product_source: IdentitySource::SetupEndpoint,
            pnp_id: None,
        }
    }

    #[test]
    fn cast_uuid_normalization_is_narrow_and_representation_independent() {
        assert_eq!(
            normalize_cast_device_id("UUID:00112233-4455-6677-8899-AABBCCDDEEFF").unwrap(),
            FIXTURE_DEVICE_ID
        );
        assert!(matches!(
            normalize_cast_device_id("friendly-but-not-an-id"),
            Err(DeviceActorError::InvalidIdentity(_))
        ));
    }
}
