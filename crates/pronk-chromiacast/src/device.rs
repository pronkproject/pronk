mod control;
mod identity;
mod preparation;

use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiacast::{AppAvailability, CastApp, CastConnection, SetupInfoOutcome, APP_MIRRORING};
use pronk_backend_protocol::{
    ControlOperation, DeviceCapabilities, MediaConfiguration, PipeWireTarget, PreparationRequest,
    SessionStatistics, StopReason, SuspendReason, Validate, MAX_ERROR_TEXT_BYTES,
    SESSION_FEATURE_CONTROL,
};
use pronk_media::{EncodedAudioPacket, EncodedVideoAccessUnit};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use zbus::zvariant::OwnedFd;

use crate::discovery::DeviceRecord;
use crate::media::{
    ChromiacastMediaSession, MediaSessionError, MediaSessionEvent, VideoEncoderPolicy,
};
use crate::transport::{
    AudioSendOutcome, AudioSenderPort, NegotiatedVideoTransport, VideoSendOutcome, VideoSenderPort,
    VideoTransportConfiguration, VideoTransportError, VideoTransportFeedbackSnapshot,
    VideoTransportNegotiator, VideoTransportPressure,
};
use identity::query_identity;
use preparation::{negotiate_capabilities, retain_supported_layouts};

const COMMAND_QUEUE_CAPACITY: usize = 8;
const EVENT_QUEUE_CAPACITY: usize = 8;
const CONTROL_EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(250);
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
        let app = self
            .connection
            .launch(APP_MIRRORING)
            .await
            .map_err(|error| VideoTransportError::new(format!("launch mirroring app: {error}")))?;
        self.active_app = Some(app);
        let result = crate::cast_transport::negotiate_launched_video(
            &self.connection,
            self.active_app.as_ref().expect("launched app was recorded"),
            configuration,
        )
        .await;
        if result.is_err() {
            let _ = self.stop_video().await;
        }
        result
    }

    async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
        // Stopping crosses a network boundary.  Once the request is sent, a
        // timeout or connection error leaves the receiver's app lifetime
        // ambiguous: it may already have stopped, or the receiver may retain
        // it.  Do not retain a stale local handle that would prevent the next
        // media generation from negotiating a fresh mirroring app.
        let Some(app) = self.active_app.take() else {
            return Ok(());
        };
        self.connection
            .stop(&app)
            .await
            .map_err(|error| VideoTransportError::new(format!("stop Cast mirroring app: {error}")))
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
        control::transmit(&self.connection, operation).await
    }

    async fn close(mut self: Box<Self>) -> Result<(), DeviceControlError> {
        // Device shutdown must release its local control owner immediately.
        // A receiver STOP or graceful Cast connection close can wait on remote
        // I/O, so leave that best-effort work to the connection task after its
        // sender is dropped instead of awaiting it in the shutdown path.
        self.active_app.take();
        Ok(())
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
                    receiver_playout_delay: None,
                    nack_count: 0,
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
        video_codec: pronk_media::VideoCodec::Vp8,
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
    owner_dropped: Option<oneshot::Sender<()>>,
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
        encoder_policy: VideoEncoderPolicy,
    ) -> Result<(Self, DeviceActorHandle, DeviceEventReceivers), DeviceActorError> {
        let media = ChromiacastMediaSession::spawn(session_id, session_generation, encoder_policy)?;
        let feedback = media.subscribe_feedback()?;
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let (bitrate_request_tx, bitrate_request_rx) = watch::channel(None);
        let (fatal_error_tx, fatal_error_rx) = oneshot::channel();
        let (owner_dropped, owner_drop_signal) = oneshot::channel();
        let handle = DeviceActorHandle {
            commands: command_tx,
        };
        let task = tokio::spawn(run_actor(DeviceTaskContext {
            device,
            allowed_features,
            connector,
            media,
            commands: command_rx,
            owner_drop_signal,
            feedback,
            events: DeviceEventSink {
                events: event_tx,
                bitrate_requests: bitrate_request_tx,
                fatal_error: Some(fatal_error_tx),
            },
        }));
        Ok((
            Self {
                handle: handle.clone(),
                owner_dropped: Some(owner_dropped),
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
        self.owner_dropped.take();
        // Let the task run shutdown_device even when another handle keeps
        // the command channel open after its owner disappears.
        self.task.take();
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

struct DeviceTaskContext {
    device: DeviceRecord,
    allowed_features: u64,
    connector: Arc<dyn DeviceConnector>,
    media: ChromiacastMediaSession,
    commands: mpsc::Receiver<DeviceCommand>,
    owner_drop_signal: oneshot::Receiver<()>,
    feedback: watch::Receiver<crate::sender_actor::VideoSenderFeedbackSnapshot>,
    events: DeviceEventSink,
}

async fn run_actor(context: DeviceTaskContext) {
    let DeviceTaskContext {
        device,
        allowed_features,
        connector,
        mut media,
        mut commands,
        mut owner_drop_signal,
        mut feedback,
        mut events,
    } = context;
    let mut control = None;
    let mut next_control_operation = 1_u64;
    let mut feedback_open = true;
    loop {
        let next = if feedback_open {
            tokio::select! {
                biased;
                _ = &mut owner_drop_signal => break,
                command = commands.recv() => NextDeviceInput::Command(command),
                changed = feedback.changed() => NextDeviceInput::Feedback(changed),
            }
        } else {
            tokio::select! {
                biased;
                _ = &mut owner_drop_signal => break,
                command = commands.recv() => NextDeviceInput::Command(command),
            }
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
                let result = if matches!(
                    reason,
                    StopReason::DisplayRemoved | StopReason::BackendShutdown
                ) {
                    // The Device session is about to be destroyed. Tear down
                    // local media owners now; closing the control owner then
                    // releases the receiver without waiting for its reply.
                    media
                        .abort_media(media_generation)
                        .await
                        .map_err(DeviceActorError::from)
                } else {
                    match control.as_deref_mut() {
                        Some(control) => media
                            .stop_media(media_generation, control)
                            .await
                            .map_err(DeviceActorError::from),
                        None => Err(DeviceActorError::InvalidRequest(
                            "StopMedia requires a prepared device connection".into(),
                        )),
                    }
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
                let completion = DeviceEvent::ControlCompleted {
                    session_generation: media.session_generation(),
                    operation_id,
                    succeeded,
                    error_text,
                };
                tokio::select! {
                    biased;
                    _ = &mut owner_drop_signal => break,
                    result = tokio::time::timeout(
                        CONTROL_EVENT_SEND_TIMEOUT,
                        events.events.send(completion),
                    ) => {
                        if result.is_err() {
                            tracing::warn!(operation_id, "Cast control completion queue remained full");
                        }
                    }
                }
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
    commands.close();
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
    mut request: PreparationRequest,
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
    let supported_layouts = media.supported_video_layouts(&request.candidate_modes)?;
    retain_supported_layouts(&mut request, supported_layouts);
    if request.candidate_modes.is_empty() {
        return Err(DeviceActorError::NoSupportedMode);
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
    let capabilities = match negotiate_capabilities(request, identity, media.raw_video_layouts()) {
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
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use pronk_backend_protocol::{
        AudioProfile, ControlKind, DeviceAvailability, DeviceInfo, DisplayIdentity, DisplayMode,
        IdentitySource, ModeRawLayouts, RawVideoLayout, VideoProfile, MAX_PRODUCT_NAME_BYTES,
        SESSION_FEATURE_AUDIO,
    };
    use pronk_media::{VideoEncoder, VideoFrameDependency, OPUS_SAMPLE_RATE};

    use super::identity::normalize_cast_device_id;
    use super::preparation::retain_supported_modes;
    use super::*;
    use crate::discovery::FIXTURE_DEVICE_ID;
    use crate::media::chromecast_video_cadence;

    fn system_layout() -> RawVideoLayout {
        RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))
    }

    fn graphics_layout() -> RawVideoLayout {
        RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9)
    }

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

    #[derive(Debug)]
    struct CloseReportingConnector {
        close_calls: Arc<AtomicUsize>,
    }

    #[derive(Debug)]
    struct CloseReportingControl {
        close_calls: Arc<AtomicUsize>,
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
    impl DeviceConnector for CloseReportingConnector {
        async fn connect(
            &self,
            _endpoint: SocketAddr,
        ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
            Ok(Box::new(CloseReportingControl {
                close_calls: Arc::clone(&self.close_calls),
            }))
        }
    }

    #[async_trait]
    impl VideoTransportNegotiator for CloseReportingControl {
        async fn negotiate_video(
            &mut self,
            configuration: VideoTransportConfiguration,
        ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
            FixtureDeviceControl.negotiate_video(configuration).await
        }

        async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[async_trait]
    impl DeviceControl for CloseReportingControl {
        async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError> {
            FixtureDeviceControl.get_device_info().await
        }

        async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError> {
            FixtureDeviceControl.get_setup_info().await
        }

        async fn get_mirroring_availability(
            &self,
        ) -> Result<MirroringAvailability, DeviceControlError> {
            FixtureDeviceControl.get_mirroring_availability().await
        }

        async fn close(self: Box<Self>) -> Result<(), DeviceControlError> {
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
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
            mode_raw_layouts: Vec::new(),
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 1_920,
                max_height: 1_080,
                max_refresh_millihz: 60_000,
                raw_layouts: vec![pronk_backend_protocol::RawVideoLayout::system_memory(
                    u32::from_le_bytes(*b"XR24"),
                )],
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
            VideoEncoderPolicy::Software,
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
    async fn dropped_owner_closes_prepared_control_with_live_command_handle() {
        let close_calls = Arc::new(AtomicUsize::new(0));
        let (actor, handle, receivers) = DeviceActor::spawn(
            device(),
            "12345678-1234-1234-1234-123456789abc".into(),
            1,
            0,
            Arc::new(CloseReportingConnector {
                close_calls: Arc::clone(&close_calls),
            }),
            VideoEncoderPolicy::Software,
        )
        .unwrap();
        handle.prepare(request()).await.unwrap();

        drop(actor);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receivers.fatal_error)
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
        assert_eq!(handle.statistics().await, Err(DeviceActorError::Stopped));
    }

    #[tokio::test]
    async fn fixture_control_uses_monotonic_ids_and_terminal_events() {
        let (actor, handle, mut receivers) = DeviceActor::spawn(
            device(),
            "12345678-1234-1234-1234-123456789abc".into(),
            7,
            SESSION_FEATURE_CONTROL,
            Arc::new(FixtureDeviceConnector),
            VideoEncoderPolicy::Software,
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

    #[tokio::test]
    async fn full_control_event_queue_does_not_block_device_shutdown() {
        let (actor, handle, receivers) = DeviceActor::spawn(
            device(),
            "12345678-1234-1234-1234-123456789abc".into(),
            7,
            SESSION_FEATURE_CONTROL,
            Arc::new(FixtureDeviceConnector),
            VideoEncoderPolicy::Software,
        )
        .unwrap();
        let mut offer = request();
        offer.requested_features = SESSION_FEATURE_CONTROL;
        handle.prepare(offer).await.unwrap();

        for operation_id in 1..=EVENT_QUEUE_CAPACITY {
            assert_eq!(
                handle
                    .transmit_control(ControlOperation {
                        session_generation: 7,
                        kind: ControlKind::Mute,
                        code: Some("on".into()),
                        value: 0,
                    })
                    .await
                    .unwrap(),
                operation_id as u64
            );
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while receivers.events.len() < EVENT_QUEUE_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle
            .transmit_control(ControlOperation {
                session_generation: 7,
                kind: ControlKind::Mute,
                code: Some("on".into()),
                value: 0,
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), actor.shutdown())
            .await
            .unwrap()
            .unwrap();
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

        let capabilities =
            negotiate_capabilities(offer, display_identity(), &[system_layout()]).unwrap();
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
        let capabilities =
            negotiate_capabilities(supported_request, display_identity(), &[system_layout()])
                .unwrap();
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
            negotiate_capabilities(unsupported_request, display_identity(), &[system_layout()],),
            Err(DeviceActorError::NoSupportedAudioProfile)
        );
    }

    #[test]
    fn video_capability_selects_one_exact_encoder_layout() {
        let mut offer = request();
        offer.video_profiles[0].raw_layouts = vec![system_layout(), graphics_layout()];

        for layout in [system_layout(), graphics_layout()] {
            let capabilities =
                negotiate_capabilities(offer.clone(), display_identity(), &[layout]).unwrap();
            assert_eq!(capabilities.video_profiles[0].raw_layouts, [layout]);
        }

        offer.video_profiles[0].raw_layouts = vec![system_layout()];
        assert_eq!(
            negotiate_capabilities(offer, display_identity(), &[graphics_layout()]),
            Err(DeviceActorError::NoSupportedVideoProfile)
        );
    }

    #[test]
    fn a_selected_gpu_layout_retains_only_compatible_display_modes() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        offer.candidate_modes = vec![large, small];
        offer.video_profiles[0].max_width = large.width;
        offer.video_profiles[0].max_height = large.height;
        offer.video_profiles[0].raw_layouts = vec![system_layout(), graphics_layout()];
        offer.mode_raw_layouts = vec![
            pronk_backend_protocol::ModeRawLayouts {
                mode: large,
                raw_layouts: vec![system_layout()],
            },
            pronk_backend_protocol::ModeRawLayouts {
                mode: small,
                raw_layouts: vec![system_layout(), graphics_layout()],
            },
        ];
        offer.validate().unwrap();
        let graphics =
            negotiate_capabilities(offer.clone(), display_identity(), &[graphics_layout()])
                .unwrap();
        assert_eq!(graphics.modes, [small]);
        assert_eq!(graphics.video_profiles[0].raw_layouts, [graphics_layout()]);
        let software =
            negotiate_capabilities(offer, display_identity(), &[system_layout()]).unwrap();
        assert_eq!(software.modes, [large, small]);
    }

    #[test]
    fn encoder_layout_selection_preserves_the_most_modes() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        let broad_layout = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AB24"), 9);
        offer.candidate_modes = vec![large, small];
        offer.video_profiles[0].max_width = large.width;
        offer.video_profiles[0].max_height = large.height;
        offer.video_profiles[0].raw_layouts = vec![graphics_layout(), broad_layout];
        offer.mode_raw_layouts = vec![
            pronk_backend_protocol::ModeRawLayouts {
                mode: large,
                raw_layouts: vec![broad_layout],
            },
            pronk_backend_protocol::ModeRawLayouts {
                mode: small,
                raw_layouts: vec![graphics_layout(), broad_layout],
            },
        ];
        offer.validate().unwrap();
        let capabilities = negotiate_capabilities(
            offer,
            display_identity(),
            &[graphics_layout(), broad_layout],
        )
        .unwrap();
        assert_eq!(capabilities.video_profiles[0].raw_layouts, [broad_layout]);
        assert_eq!(capabilities.modes, [large, small]);
    }

    #[test]
    fn encoder_profile_selection_preserves_the_most_modes() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        offer.candidate_modes.insert(0, large);
        let mut broad = offer.video_profiles[0].clone();
        broad.profile_id = "h264-large".into();
        broad.max_width = large.width;
        broad.max_height = large.height;
        offer.video_profiles.push(broad);
        offer.validate().unwrap();

        let capabilities =
            negotiate_capabilities(offer, display_identity(), &[system_layout()]).unwrap();
        assert_eq!(capabilities.video_profiles[0].profile_id, "h264-large");
        assert_eq!(capabilities.modes, [large, small]);
    }

    #[test]
    fn encoder_mode_filter_also_narrows_per_mode_layouts() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        offer.candidate_modes.insert(0, large);
        offer.mode_raw_layouts = vec![
            pronk_backend_protocol::ModeRawLayouts {
                mode: large,
                raw_layouts: vec![system_layout()],
            },
            pronk_backend_protocol::ModeRawLayouts {
                mode: small,
                raw_layouts: vec![system_layout()],
            },
        ];
        retain_supported_modes(&mut offer, vec![small]);
        offer.validate().unwrap();
        assert_eq!(offer.candidate_modes, [small]);
        assert_eq!(offer.mode_raw_layouts[0].mode, small);
        assert_eq!(offer.mode_raw_layouts.len(), 1);
    }

    #[test]
    fn encoder_format_limits_apply_to_each_offered_mode() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        let ar = graphics_layout();
        let ab = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AB24"), 9);
        offer.candidate_modes.insert(0, large);
        offer.video_profiles[0].raw_layouts = vec![ar, ab];
        offer.mode_raw_layouts = vec![
            ModeRawLayouts {
                mode: large,
                raw_layouts: vec![ar, ab],
            },
            ModeRawLayouts {
                mode: small,
                raw_layouts: vec![ar],
            },
        ];
        offer.validate().unwrap();

        retain_supported_layouts(&mut offer, vec![vec![ab], vec![ar]]);
        assert_eq!(offer.candidate_modes, [large, small]);
        assert_eq!(offer.mode_raw_layouts[0].raw_layouts, [ab]);
        assert_eq!(offer.mode_raw_layouts[1].raw_layouts, [ar]);

        retain_supported_layouts(&mut offer, vec![vec![ar], vec![ar]]);
        assert_eq!(offer.candidate_modes, [small]);
        assert_eq!(offer.mode_raw_layouts[0].raw_layouts, [ar]);
    }

    #[test]
    fn backend_formats_narrow_an_unrestricted_mode_offer() {
        let mut offer = request();
        offer.video_profiles[0].raw_layouts = vec![system_layout(), graphics_layout()];
        offer.validate().unwrap();
        retain_supported_layouts(&mut offer, vec![vec![graphics_layout()]]);
        assert_eq!(offer.mode_raw_layouts[0].raw_layouts, [graphics_layout()]);
    }

    #[tokio::test]
    #[ignore = "requires PRONK_GPU_RENDER_NODE and a matching VA converter"]
    async fn selected_va_device_prepares_only_its_usable_mode_formats() {
        let render_node = std::env::var_os("PRONK_GPU_RENDER_NODE")
            .expect("PRONK_GPU_RENDER_NODE names the selected VA render node");
        let render_node = std::path::PathBuf::from(render_node);
        let metadata = std::fs::metadata(&render_node).unwrap();
        let render_device = pronk_backend_protocol::RenderDeviceIdentity {
            major: u32::try_from(nix::sys::stat::major(metadata.rdev())).unwrap(),
            minor: u32::try_from(nix::sys::stat::minor(metadata.rdev())).unwrap(),
        };
        let encoder = VideoEncoder::va_h264(&render_node);
        let raw_layouts: Vec<_> = encoder
            .supported_dma_buf_formats(chromecast_video_cadence())
            .unwrap()
            .into_iter()
            .filter(|format| {
                [b"AR24", b"XR24", b"AB24", b"XB24"]
                    .iter()
                    .any(|fourcc| format.format == u32::from_le_bytes(**fourcc))
            })
            .map(|format| RawVideoLayout::dma_buf(format.format, format.modifier))
            .collect();
        assert!(!raw_layouts.is_empty());
        // Source allocation is qualified by the separate GPU media fixture.
        // Here the complete presentation offer exercises backend preparation.
        let mut offer = request();
        offer.candidate_modes = [
            (3840, 2160, 30_000),
            (2560, 1440, 60_000),
            (1920, 1080, 60_000),
            (1600, 900, 60_000),
            (1366, 768, 60_000),
            (1280, 720, 60_000),
            (640, 480, 60_000),
        ]
        .into_iter()
        .map(|(width, height, refresh_millihz)| DisplayMode {
            width,
            height,
            refresh_millihz,
            flags: 0,
        })
        .collect();
        offer.video_profiles[0].max_width = 3840;
        offer.video_profiles[0].max_height = 2160;
        offer.video_profiles[0].raw_layouts = raw_layouts.clone();
        offer.validate().unwrap();
        let supported = encoder
            .supported_dma_buf_formats_for_dimensions(
                &offer
                    .candidate_modes
                    .iter()
                    .map(|mode| (mode.width, mode.height))
                    .collect::<Vec<_>>(),
                chromecast_video_cadence(),
            )
            .unwrap();
        let most_modes = raw_layouts
            .iter()
            .map(|layout| {
                supported
                    .iter()
                    .filter(|formats| {
                        formats.iter().any(|format| {
                            *layout == RawVideoLayout::dma_buf(format.format, format.modifier)
                        })
                    })
                    .count()
            })
            .max()
            .unwrap();
        assert!(most_modes > 0);
        let offered_modes = offer.candidate_modes.clone();
        let (actor, handle, _events) = DeviceActor::spawn(
            device(),
            "12345678-1234-1234-1234-123456789abc".into(),
            1,
            0,
            Arc::new(ScriptedConnector {
                device_id: FIXTURE_DEVICE_ID,
                setup: Ok(ControlSetupInfo::Unsupported),
            }),
            VideoEncoderPolicy::VaH264 {
                render_node,
                render_device,
                raw_layouts,
                minimum_bitrate: 0,
                maximum_bitrate: u64::MAX,
            },
        )
        .unwrap();
        let capabilities = handle.prepare(offer).await.unwrap();
        eprintln!(
            "selected VA backend retained modes: {:?}",
            capabilities.modes
        );
        assert!(capabilities
            .modes
            .iter()
            .any(|mode| { (mode.width, mode.height, mode.refresh_millihz) == (640, 480, 60_000) }));
        let layout = capabilities.video_profiles[0].raw_layouts[0];
        assert_eq!(capabilities.modes.len(), most_modes);
        for mode in &capabilities.modes {
            let index = offered_modes
                .iter()
                .position(|offered| offered == mode)
                .unwrap();
            assert!(supported[index].iter().any(|format| {
                layout == RawVideoLayout::dma_buf(format.format, format.modifier)
            }));
        }
        actor.shutdown().await.unwrap();
    }

    #[test]
    fn video_profile_limits_exclude_larger_display_modes() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        offer.candidate_modes.insert(0, large);
        let capabilities =
            negotiate_capabilities(offer, display_identity(), &[system_layout()]).unwrap();
        assert_eq!(capabilities.modes, [small]);
    }

    #[test]
    fn unavailable_large_only_layout_does_not_hide_a_usable_fallback() {
        let mut offer = request();
        let large = DisplayMode {
            width: 3_840,
            height: 2_160,
            refresh_millihz: 30_000,
            flags: 0,
        };
        let small = offer.candidate_modes[0];
        offer.candidate_modes.insert(0, large);
        offer.video_profiles[0].raw_layouts = vec![graphics_layout(), system_layout()];
        offer.mode_raw_layouts = vec![
            pronk_backend_protocol::ModeRawLayouts {
                mode: large,
                raw_layouts: vec![graphics_layout()],
            },
            pronk_backend_protocol::ModeRawLayouts {
                mode: small,
                raw_layouts: vec![system_layout()],
            },
        ];
        offer.validate().unwrap();
        let capabilities = negotiate_capabilities(
            offer,
            display_identity(),
            &[graphics_layout(), system_layout()],
        )
        .unwrap();
        assert_eq!(capabilities.modes, [small]);
        assert_eq!(
            capabilities.video_profiles[0].raw_layouts,
            [system_layout()]
        );
    }

    #[test]
    fn video_capability_respects_the_source_layout_order() {
        let mut offer = request();
        let argb = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9);
        let abgr = RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AB24"), 9);
        offer.video_profiles[0].raw_layouts = vec![argb, abgr];
        let capabilities =
            negotiate_capabilities(offer, display_identity(), &[abgr, argb]).unwrap();
        assert_eq!(capabilities.video_profiles[0].raw_layouts, [argb]);
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
