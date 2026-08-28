//! Sole-owner CastKMS capture and PipeWire producer actor.
//!
//! One task owns the only grant-holder fd, every capture ioctl/event, and the
//! per-display PipeWire source actor. Callers receive command handles and
//! exact node identity, never a DRM fd or grant capability.

use std::collections::{HashMap, VecDeque};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nix::errno::Errno;
use pronk_core::castkms::{
    AsyncCastKmsClient, CaptureBufferInfo, CaptureBufferState, CaptureError, CaptureFrameEvent,
    CaptureQueue, CaptureStreamInfo, CaptureSynchronization, CastKmsClient, CastKmsEvent,
    CecCompletion, CecTransmitAdmission, CecTransmitEvent, CursorCaptureMode, ExplicitCaptureFence,
    GrantStateEvidence, MAX_OUTSTANDING_CAPTURE_REQUESTS,
};
use pronk_pipewire::{
    CastKmsAudioSinkRequest, CastKmsAudioSinkResolver, ClassifiedSocketRemoteProvider,
    PipeWireBufferTransport, VideoBuffer, VideoBufferLayout, VideoDamage, VideoFrame,
    VideoNodeIdentity, VideoSourceActor, VideoSourceActorError, VideoSourceActorEvent,
    VideoSourceConfig, VideoSourceGeneration, VideoSyncTimelines,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::cec_bridge::{CecBridge, CecBridgeAction};
use crate::device_control_port::{DeviceControlError, DeviceControlPort};
use crate::device_session_port::{DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaTarget};
use crate::kernel_display::{query_observation, DEFAULT_TOPOLOGY_POLL_INTERVAL};
use crate::kernel_display_port::{
    KernelDisplayError, KernelDisplayEvent, KernelDisplayMetadata, KernelDisplayObservation,
    KernelDisplayPort,
};
use crate::media_pipeline_port::{CapturePipelinePort, MediaPipelineError, PreparedCaptureMedia};
use crate::media_session::{MediaStartRequest, MediaStopReason, MediaSuspendReason};

const COMMAND_CAPACITY: usize = 16;
const CAPTURE_POOL_SIZE: usize = 4;
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_QUEUE_RETRY_INTERVAL: Duration = Duration::from_millis(2);
const CEC_CONTROL_TIMEOUT: Duration = Duration::from_millis(1_750);

#[derive(Debug, Clone)]
pub struct CastKmsActorConfig {
    pub producer_remotes: ClassifiedSocketRemoteProvider,
    pub session_id: String,
    pub device_instance: String,
    pub node_description: String,
    pub device_path: PathBuf,
    pub output_index: u32,
    pub audio_sink_resolver: CastKmsAudioSinkResolver,
    pub video_profile_id: String,
    pub audio_profile_id: Option<String>,
    pub video_bitrate: NonZeroU64,
    pub device_control: Option<Arc<dyn DeviceControlPort>>,
}

/// Owner half consumed by the display slot.
pub struct CastKmsKernelActor {
    metadata: KernelDisplayMetadata,
    initial: KernelDisplayObservation,
    commands: mpsc::Sender<Command>,
    events: mpsc::UnboundedReceiver<KernelDisplayEvent>,
    task: Option<JoinHandle<Result<(), KernelDisplayError>>>,
}

impl std::fmt::Debug for CastKmsKernelActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CastKmsKernelActor")
            .field("metadata", &self.metadata)
            .field("initial", &self.initial)
            .finish_non_exhaustive()
    }
}

/// Command-only half consumed by the production media driver.
#[derive(Debug, Clone)]
pub struct CastKmsCapturePort {
    commands: mpsc::Sender<Command>,
}

impl CastKmsKernelActor {
    pub fn spawn(
        mut client: CastKmsClient,
        config: CastKmsActorConfig,
    ) -> Result<(Self, CastKmsCapturePort), KernelDisplayError> {
        if client.output_index() != config.output_index {
            return Err(KernelDisplayError::new(
                "validate connector output identity",
                format!(
                    "grant output index {} differs from reserved output {}",
                    client.output_index(),
                    config.output_index
                ),
            ));
        }
        let initial = query_observation(&client)?;
        let metadata = KernelDisplayMetadata {
            grant_id: client.grant_id(),
        };
        if config.device_control.is_some() {
            let capabilities = client.query_cec_capabilities().map_err(|error| {
                KernelDisplayError::new("query connector CEC capabilities", error.to_string())
            })?;
            if let Some(capabilities) = capabilities {
                if capabilities.output_index() != config.output_index {
                    return Err(KernelDisplayError::new(
                        "validate connector CEC identity",
                        format!(
                            "CEC output index {} differs from reserved output {}",
                            capabilities.output_index(),
                            config.output_index
                        ),
                    ));
                }
                client.bind_cec_transport(capabilities).map_err(|error| {
                    KernelDisplayError::new("bind connector CEC transport", error.to_string())
                })?;
                client.set_cec_transport_online(true).map_err(|error| {
                    KernelDisplayError::new(
                        "bring connector CEC transport online",
                        error.to_string(),
                    )
                })?;
            } else {
                tracing::debug!(
                    output_index = config.output_index,
                    "CastKMS connector has no CEC adapter; continue without a CEC bridge"
                );
            }
        }
        let client = client.into_async().map_err(|error| {
            KernelDisplayError::new(
                "register grant holder for Tokio readiness",
                error.to_string(),
            )
        })?;
        let video = VideoSourceActor::spawn().map_err(|error| {
            KernelDisplayError::new("spawn per-display PipeWire source actor", error.to_string())
        })?;
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::unbounded_channel();
        let task_commands = commands.clone();
        let actor_session_id = config.session_id.clone();
        let task = tokio::spawn(async move {
            let result = run_actor(client, video, config, command_rx, event_tx).await;
            if let Err(error) = &result {
                tracing::error!(
                    session_id = %actor_session_id,
                    %error,
                    "CastKMS kernel actor stopped"
                );
            }
            result
        });
        Ok((
            Self {
                metadata,
                initial,
                commands: task_commands,
                events,
                task: Some(task),
            },
            CastKmsCapturePort { commands },
        ))
    }

    async fn join_task(&mut self) -> Result<(), KernelDisplayError> {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|error| KernelDisplayError::new("join CastKMS actor", error.to_string()))?
    }
}

impl Drop for CastKmsKernelActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // The orderly path is `detach`. Aborting drops the sole holder and
            // PipeWire actor fail-closed instead of orphaning either task.
            task.abort();
        }
    }
}

#[async_trait]
impl KernelDisplayPort for CastKmsKernelActor {
    fn metadata(&self) -> KernelDisplayMetadata {
        self.metadata
    }

    fn initial_observation(&self) -> KernelDisplayObservation {
        self.initial
    }

    async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError> {
        self.events
            .recv()
            .await
            .ok_or_else(|| KernelDisplayError::new("observe CastKMS actor", "event stream closed"))
    }

    async fn detach(mut self: Box<Self>) -> Result<(), KernelDisplayError> {
        let (reply, response) = oneshot::channel();
        let send_result = self.commands.send(Command::Detach { reply }).await;
        if send_result.is_err() {
            return self.join_task().await;
        }
        let result = response.await.map_err(|_| {
            KernelDisplayError::new("detach grant-scoped monitor", "actor reply closed")
        })?;
        let joined = self.join_task().await;
        result.and(joined)
    }
}

#[async_trait]
impl CapturePipelinePort for CastKmsCapturePort {
    async fn start(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<PreparedCaptureMedia, MediaPipelineError> {
        let (reply, response) = oneshot::channel();
        send_command(
            &self.commands,
            Command::Start {
                request,
                cancellation: cancellation.clone(),
                reply,
            },
            &cancellation,
        )
        .await?;
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                Err(MediaPipelineError::new("CastKMS capture start was cancelled"))
            }
            result = response => result
                .map_err(|_| MediaPipelineError::new("CastKMS actor reply closed"))?
                .map_err(|error| MediaPipelineError::new(error.to_string())),
        }
    }

    async fn activate(
        &mut self,
        media_generation: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        request_unit(
            &self.commands,
            &cancellation,
            |reply| Command::Activate {
                media_generation,
                reply,
            },
            "activate CastKMS capture",
        )
        .await
    }

    async fn suspend(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaSuspendReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        request_unit(
            &self.commands,
            &cancellation,
            |reply| Command::Suspend {
                media_generation,
                reply,
            },
            "suspend CastKMS capture",
        )
        .await
    }

    async fn stop(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        request_unit(
            &self.commands,
            &cancellation,
            |reply| Command::Stop {
                media_generation,
                reply,
            },
            "stop CastKMS capture",
        )
        .await
    }

    async fn shutdown(
        &mut self,
        _reason: MediaStopReason,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        // The slot still owns attachment and the sole actor. Final Device
        // shutdown occurs first; its subsequent KernelDisplayPort::detach is
        // the command that terminates this actor.
        if cancellation.is_cancelled() {
            Err(MediaPipelineError::new(
                "CastKMS capture shutdown was cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

async fn send_command(
    commands: &mpsc::Sender<Command>,
    command: Command,
    cancellation: &CancellationToken,
) -> Result<(), MediaPipelineError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            Err(MediaPipelineError::new("CastKMS command was cancelled"))
        }
        result = commands.send(command) => result
            .map_err(|_| MediaPipelineError::new("CastKMS actor command channel closed")),
    }
}

async fn request_unit(
    commands: &mpsc::Sender<Command>,
    cancellation: &CancellationToken,
    make: impl FnOnce(oneshot::Sender<Result<(), KernelDisplayError>>) -> Command,
    operation: &'static str,
) -> Result<(), MediaPipelineError> {
    let (reply, response) = oneshot::channel();
    send_command(commands, make(reply), cancellation).await?;
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            Err(MediaPipelineError::new(format!("{operation} was cancelled")))
        }
        result = response => result
            .map_err(|_| MediaPipelineError::new("CastKMS actor reply closed"))?
            .map_err(|error| MediaPipelineError::new(error.to_string())),
    }
}

#[derive(Debug)]
enum Command {
    Start {
        request: MediaStartRequest,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<PreparedCaptureMedia, KernelDisplayError>>,
    },
    Activate {
        media_generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), KernelDisplayError>>,
    },
    Suspend {
        media_generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), KernelDisplayError>>,
    },
    Stop {
        media_generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), KernelDisplayError>>,
    },
    Detach {
        reply: oneshot::Sender<Result<(), KernelDisplayError>>,
    },
}

#[derive(Debug)]
struct ActiveGeneration {
    media_generation: NonZeroU64,
    stream: CaptureStreamInfo,
    identity: VideoNodeIdentity,
    transports: HashMap<NonZeroU32, PipeWireBufferTransport>,
    available: VecDeque<NonZeroU32>,
    outstanding: VecDeque<OutstandingCapture>,
    next_user_data: u64,
    source_active: bool,
    suspended: bool,
}

#[derive(Debug)]
struct OutstandingCapture {
    queue: CaptureQueue,
    fence: ExplicitCaptureFence,
    transport: PipeWireBufferTransport,
}

#[derive(Debug)]
struct PendingCecControl {
    event: CecTransmitEvent,
    task: JoinHandle<Result<(), DeviceControlError>>,
}

impl Drop for PendingCecControl {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
enum QueueFailure {
    Recoverable(KernelDisplayError),
    /// Queue succeeded but the actor could not mint the evidence required to
    /// reclaim that buffer. Dropping the sole owner is the only safe path.
    Fatal(KernelDisplayError),
}

async fn run_actor(
    mut client: AsyncCastKmsClient,
    mut video: VideoSourceActor,
    config: CastKmsActorConfig,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::UnboundedSender<KernelDisplayEvent>,
) -> Result<(), KernelDisplayError> {
    let mut active = None;
    let mut cec_bridge = (config.device_control.is_some() && client.client().cec_transport_bound())
        .then(CecBridge::new);
    let mut pending_cec_control: Option<PendingCecControl> = None;
    let mut current = query_observation(client.client())?;
    let mut poll = interval(DEFAULT_TOPOLOGY_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    poll.reset();
    let mut capture_queue_retry = interval(CAPTURE_QUEUE_RETRY_INTERVAL);
    capture_queue_retry.set_missed_tick_behavior(MissedTickBehavior::Skip);
    capture_queue_retry.reset();

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Start { request, cancellation, reply }) => {
                    let result = if active.is_some() {
                        Err(KernelDisplayError::new(
                            "start capture generation",
                            "another media generation is still active",
                        ))
                    } else {
                        start_generation(&mut client, &video, &config, request, &cancellation)
                            .await
                            .map(|(generation, prepared)| {
                                active = Some(generation);
                                prepared
                            })
                    };
                    let incomplete_rollback = result.is_err()
                        && (client.client().active_capture_stream().is_some()
                            || client.client().retired_capture_stream().is_some());
                    if let Err(returned) = reply.send(result) {
                        if returned.is_ok() {
                            stop_active(&mut client, &video, &mut active).await?;
                        }
                    }
                    if incomplete_rollback {
                        return Err(KernelDisplayError::new(
                            "roll back failed capture start",
                            "capture resources remain owned after startup failure",
                        ));
                    }
                }
                Some(Command::Activate { media_generation, reply }) => {
                    let result = require_generation(active.as_mut(), media_generation)
                        .map(|generation| generation.suspended = false);
                    let _ = reply.send(result);
                }
                Some(Command::Suspend { media_generation, reply }) => {
                    let result = require_generation(active.as_mut(), media_generation)
                        .map(|generation| generation.suspended = true);
                    let _ = reply.send(result);
                }
                Some(Command::Stop { media_generation, reply }) => {
                    let result = match active.as_ref() {
                        None => Ok(()),
                        Some(generation) if generation.media_generation == media_generation => {
                            stop_active(&mut client, &video, &mut active).await
                        }
                        Some(generation) => Err(KernelDisplayError::new(
                            "stop capture generation",
                            format!(
                                "requested generation {media_generation}; active generation is {}",
                                generation.media_generation
                            ),
                        )),
                    };
                    let _ = reply.send(result);
                }
                Some(Command::Detach { reply }) => {
                    let mut failures = Vec::new();
                    if let Err(error) = stop_active(&mut client, &video, &mut active).await {
                        failures.push(error.to_string());
                    }
                    if let Err(error) = video.shutdown().await {
                        failures.push(format!("shut down PipeWire source actor: {error}"));
                    }
                    teardown_cec(&mut client, &mut pending_cec_control, &mut failures).await;
                    let client = client.into_client();
                    let detached = tokio::task::spawn_blocking(move || client.detach_monitor())
                        .await
                        .map_err(|error| {
                            KernelDisplayError::new("join CastKMS detach", error.to_string())
                        })
                        .and_then(|result| {
                            result.map_err(|error| {
                                KernelDisplayError::new(
                                    "detach grant-scoped monitor",
                                    error.to_string(),
                                )
                            })
                        });
                    if let Err(error) = detached {
                        failures.push(error.to_string());
                    }
                    let result = combined_kernel_failures("tear down CastKMS actor", failures);
                    let _ = reply.send(result);
                    return Ok(());
                }
                None => return Err(KernelDisplayError::new(
                    "run CastKMS actor",
                    "all command handles closed without orderly detach",
                )),
            },
            _ = poll.tick() => {
                let observation = query_observation(client.client())?;
                let grant_changed = observation.grant_state != current.grant_state;
                if grant_changed
                    || (observation.grant_state
                        != crate::display_state::DisplayGrantState::Active
                        && active.is_some())
                {
                    client
                        .client_mut()
                        .reconcile_grant_state(GrantStateEvidence::Query)
                        .map_err(|error| {
                            KernelDisplayError::new(
                                "reconcile CastKMS grant state",
                                error.to_string(),
                            )
                        })?;
                }
                if grant_changed {
                    synchronize_cec_authority(
                        &mut client,
                        observation.grant_state
                            == crate::display_state::DisplayGrantState::Active,
                        &mut pending_cec_control,
                    )?;
                }
                publish_observation(&events, &mut current, observation);
                if observation.grant_state != crate::display_state::DisplayGrantState::Active
                    && active.is_some()
                {
                    fail_active(
                        &mut client,
                        &video,
                        &mut active,
                        &events,
                        format!("CastKMS grant became {:?}", observation.grant_state),
                    )
                    .await?;
                }
            }
            result = client.read_events() => {
                let batch = result.map_err(|error| {
                    KernelDisplayError::new("read grant-holder event stream", error.to_string())
                })?;
                for event in batch {
                    match event {
                        CastKmsEvent::CaptureFrame(frame) => {
                            match handle_capture_frame(
                                &mut client,
                                &video,
                                active.as_mut(),
                                frame,
                            )
                            .await
                            {
                                Ok(()) => match maybe_queue(&mut client, active.as_mut()) {
                                    Ok(()) => {}
                                    Err(QueueFailure::Recoverable(error)) => {
                                        fail_active(
                                            &mut client,
                                            &video,
                                            &mut active,
                                            &events,
                                            error.to_string(),
                                        )
                                        .await?;
                                    }
                                    Err(QueueFailure::Fatal(error)) => return Err(error),
                                },
                                Err(error) => {
                                    fail_active(
                                        &mut client,
                                        &video,
                                        &mut active,
                                        &events,
                                        error.to_string(),
                                    )
                                    .await?;
                                }
                            }
                        }
                        CastKmsEvent::GrantState(event) => {
                            client
                                .client_mut()
                                .reconcile_grant_state(GrantStateEvidence::Event(event))
                                .map_err(|error| {
                                    KernelDisplayError::new(
                                        "reconcile CastKMS grant-state event",
                                        error.to_string(),
                                    )
                                })?;
                            synchronize_cec_authority(
                                &mut client,
                                event.state == pronk_core::castkms::GrantState::Active,
                                &mut pending_cec_control,
                            )?;
                            let observation = query_observation(client.client())?;
                            publish_observation(&events, &mut current, observation);
                            if event.state != pronk_core::castkms::GrantState::Active
                                && active.is_some()
                            {
                                fail_active(
                                    &mut client,
                                    &video,
                                    &mut active,
                                    &events,
                                    format!("CastKMS grant became {:?}", event.state),
                                )
                                .await?;
                            }
                        }
                        CastKmsEvent::GrantRevoked(_) => {
                            synchronize_cec_authority(
                                &mut client,
                                false,
                                &mut pending_cec_control,
                            )?;
                            let _ = events.send(KernelDisplayEvent::Revoked);
                            if active.is_some() {
                                client
                                    .client_mut()
                                    .reconcile_grant_state(GrantStateEvidence::Query)
                                    .map_err(|error| {
                                        KernelDisplayError::new(
                                            "reconcile revoked CastKMS grant",
                                            error.to_string(),
                                        )
                                    })?;
                                fail_active(
                                    &mut client,
                                    &video,
                                    &mut active,
                                    &events,
                                    "CastKMS grant was revoked".into(),
                                )
                                .await?;
                            }
                        }
                        CastKmsEvent::CecTransmit(event) => {
                            if !client.client().cec_transport_online() {
                                let grant = client.client().query_grant().map_err(|error| {
                                    KernelDisplayError::new(
                                        "recheck CEC transmit authority",
                                        error.to_string(),
                                    )
                                })?;
                                client
                                    .client_mut()
                                    .reconcile_grant_state(GrantStateEvidence::Query)
                                    .map_err(|error| {
                                        KernelDisplayError::new(
                                            "reconcile CEC transmit authority",
                                            error.to_string(),
                                        )
                                    })?;
                                if grant.state == pronk_core::castkms::GrantState::Active {
                                    synchronize_cec_authority(
                                        &mut client,
                                        true,
                                        &mut pending_cec_control,
                                    )?;
                                }
                            }
                            let bridge = cec_bridge.as_mut().ok_or_else(|| {
                                KernelDisplayError::new(
                                    "handle CastKMS CEC transmit",
                                    "CEC event arrived without an enabled Device control port",
                                )
                            })?;
                            let control = config.device_control.as_ref().expect(
                                "CEC bridge and Device control are configured together",
                            );
                            handle_cec_transmit(
                                &mut client,
                                bridge,
                                Arc::clone(control),
                                &mut pending_cec_control,
                                event,
                            )?;
                        }
                        CastKmsEvent::Unknown(_) => {}
                    }
                }
            }
            control_result = async {
                let pending = pending_cec_control
                    .as_mut()
                    .expect("guarded CEC completion branch has a pending operation");
                (&mut pending.task).await
            }, if pending_cec_control.is_some() => {
                let pending = pending_cec_control
                    .take()
                    .expect("guarded CEC completion branch has a pending operation");
                let completion = match control_result {
                    Ok(Ok(())) => CecCompletion::succeeded(),
                    Ok(Err(_)) => CecCompletion::not_acknowledged(pending.event.attempts),
                    Err(_) => CecCompletion::failed(),
                };
                client
                    .client_mut()
                    .complete_cec_transmit(&pending.event, completion)
                    .map_err(|error| KernelDisplayError::new(
                        "complete Device-backed CEC transmit",
                        error.to_string(),
                    ))?;
            }
            _ = capture_queue_retry.tick(), if capture_queue_needs_retry(active.as_ref()) => {
                match maybe_queue(&mut client, active.as_mut()) {
                    Ok(()) => {}
                    Err(QueueFailure::Recoverable(error)) => {
                        fail_active(
                            &mut client,
                            &video,
                            &mut active,
                            &events,
                            error.to_string(),
                        )
                        .await?;
                    }
                    Err(QueueFailure::Fatal(error)) => return Err(error),
                }
            }
            source_event = video.next_event(), if active.is_some() => {
                let result = match source_event {
                    Some(event) => handle_source_event(&mut client, active.as_mut(), event),
                    None => Err(KernelDisplayError::new(
                        "observe PipeWire source",
                        "source actor event stream closed",
                    )),
                };
                match result {
                    Ok(()) => {
                        match maybe_queue(&mut client, active.as_mut()) {
                            Ok(()) => {}
                            Err(QueueFailure::Recoverable(error)) => {
                                fail_active(
                                    &mut client,
                                    &video,
                                    &mut active,
                                    &events,
                                    error.to_string(),
                                )
                                .await?;
                            }
                            Err(QueueFailure::Fatal(error)) => return Err(error),
                        }
                    }
                    Err(error) => {
                        fail_active(
                            &mut client,
                            &video,
                            &mut active,
                            &events,
                            error.to_string(),
                        )
                        .await?;
                    }
                }
            }
        }
    }
}

fn synchronize_cec_authority(
    client: &mut AsyncCastKmsClient,
    authority_active: bool,
    pending: &mut Option<PendingCecControl>,
) -> Result<(), KernelDisplayError> {
    if !client.client().cec_transport_bound() {
        return Ok(());
    }
    if authority_active {
        return client
            .client_mut()
            .set_cec_transport_online(true)
            .map_err(|error| {
                KernelDisplayError::new(
                    "resume connector CEC transport authority",
                    error.to_string(),
                )
            });
    }

    // CastKMS synchronously aborts any kernel transaction and forces the
    // transport offline before publishing the grant-state transition.
    // Dropping this task cancels the corresponding Device operation too.
    pending.take();
    client
        .client_mut()
        .record_cec_authority_suspended()
        .map_err(|error| {
            KernelDisplayError::new(
                "suspend connector CEC transport authority",
                error.to_string(),
            )
        })
}

fn handle_cec_transmit(
    client: &mut AsyncCastKmsClient,
    bridge: &mut CecBridge,
    control: Arc<dyn DeviceControlPort>,
    pending: &mut Option<PendingCecControl>,
    event: CecTransmitEvent,
) -> Result<(), KernelDisplayError> {
    if pending.is_some() {
        return Err(KernelDisplayError::new(
            "handle CastKMS CEC transmit",
            "a Device control operation is already pending",
        ));
    }
    let admission = client
        .client_mut()
        .record_cec_transmit(&event)
        .map_err(|error| {
            KernelDisplayError::new("record CastKMS CEC transmit", error.to_string())
        })?;
    if admission == CecTransmitAdmission::Stale {
        tracing::debug!(
            transport_generation = event.transport_generation,
            state_generation = event.state_generation,
            cookie = event.cookie,
            "ignore stale CastKMS CEC transmit"
        );
        return Ok(());
    }
    match bridge.translate(event.message()) {
        CecBridgeAction::Acknowledge => client
            .client_mut()
            .complete_cec_transmit(&event, CecCompletion::succeeded())
            .map_err(|error| {
                KernelDisplayError::new("acknowledge CastKMS CEC transmit", error.to_string())
            }),
        CecBridgeAction::NotAcknowledged => client
            .client_mut()
            .complete_cec_transmit(&event, CecCompletion::not_acknowledged(event.attempts))
            .map_err(|error| {
                KernelDisplayError::new("reject CastKMS CEC transmit", error.to_string())
            }),
        CecBridgeAction::Reply(message) => {
            client
                .client_mut()
                .complete_cec_transmit(&event, CecCompletion::succeeded())
                .map_err(|error| {
                    KernelDisplayError::new("acknowledge CastKMS CEC request", error.to_string())
                })?;
            client
                .client_mut()
                .inject_cec_message(&message)
                .map_err(|error| {
                    KernelDisplayError::new("inject CastKMS CEC response", error.to_string())
                })
        }
        CecBridgeAction::Control(operation) => {
            let task = tokio::spawn(async move {
                match timeout(CEC_CONTROL_TIMEOUT, control.transmit_control(operation)).await {
                    Ok(result) => result,
                    Err(_) => Err(DeviceControlError::new(
                        "Device control operation timed out",
                    )),
                }
            });
            *pending = Some(PendingCecControl { event, task });
            Ok(())
        }
    }
}

async fn teardown_cec(
    client: &mut AsyncCastKmsClient,
    pending: &mut Option<PendingCecControl>,
    failures: &mut Vec<String>,
) {
    if let Some(mut pending) = pending.take() {
        pending.task.abort();
        let _ = (&mut pending.task).await;
        if let Err(error) = client
            .client_mut()
            .complete_cec_transmit(&pending.event, CecCompletion::failed())
        {
            failures.push(format!("fail pending CEC transmit: {error}"));
        }
    }
    if !client.client().cec_transport_bound() {
        return;
    }
    if let Err(error) = client.client_mut().set_cec_transport_online(false) {
        failures.push(format!("take CEC transport offline: {error}"));
    }
    if let Err(error) = client.client_mut().unbind_cec_transport() {
        failures.push(format!("unbind CEC transport: {error}"));
    }
}

async fn start_generation(
    client: &mut AsyncCastKmsClient,
    video: &VideoSourceActor,
    config: &CastKmsActorConfig,
    request: MediaStartRequest,
    cancellation: &CancellationToken,
) -> Result<(ActiveGeneration, PreparedCaptureMedia), KernelDisplayError> {
    let media_generation = NonZeroU64::new(request.media_generation).ok_or_else(|| {
        KernelDisplayError::new("start capture generation", "media generation is zero")
    })?;
    let connector_id = NonZeroU32::new(client.client().connector_id()).ok_or_else(|| {
        KernelDisplayError::new("start capture generation", "CastKMS connector ID is zero")
    })?;
    let grant_id = NonZeroU32::new(client.client().grant_id()).ok_or_else(|| {
        KernelDisplayError::new("start capture generation", "CastKMS grant ID is zero")
    })?;
    require_not_cancelled(cancellation, "start capture generation")?;
    let audio_sink = if config.audio_profile_id.is_some() {
        let remote = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(KernelDisplayError::new(
                    "resolve connector-bound audio sink",
                    "operation was cancelled",
                ));
            }
            result = config.producer_remotes.create_producer_remote() => result.map_err(|error| {
                KernelDisplayError::new(
                    "connect classified PipeWire audio resolver",
                    error.to_string(),
                )
            })?,
        };
        let request = CastKmsAudioSinkRequest {
            device_path: config.device_path.clone(),
            output_index: config.output_index,
        };
        Some(tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(KernelDisplayError::new(
                    "resolve connector-bound audio sink",
                    "operation was cancelled",
                ));
            }
            result = config.audio_sink_resolver.resolve(request, remote.into_remote()) => {
                result.map_err(|error| KernelDisplayError::new(
                    "resolve connector-bound audio sink",
                    error.to_string(),
                ))?
            }
        })
    } else {
        None
    };
    require_not_cancelled(cancellation, "start capture generation")?;
    let capabilities = client
        .client()
        .query_capture_capabilities(request.route.target.as_nonzero())
        .map_err(|error| {
            KernelDisplayError::new("query CastKMS capture capabilities", error.to_string())
        })?;
    if capabilities.max_registered_buffers() < CAPTURE_POOL_SIZE as u32 {
        return Err(KernelDisplayError::new(
            "start capture generation",
            format!(
                "CastKMS supports {} buffers; {CAPTURE_POOL_SIZE} are required",
                capabilities.max_registered_buffers()
            ),
        ));
    }
    let stream = client
        .client_mut()
        .start_capture(&capabilities, CursorCaptureMode::IncludeInFrame)
        .map_err(|error| KernelDisplayError::new("start CastKMS capture", error.to_string()))?;
    if let Err(error) = validate_stream_route(stream, request) {
        rollback_unpublished(client, stream)?;
        return Err(error);
    }

    let buffers = match allocate_pool(client, stream) {
        Ok(buffers) => buffers,
        Err(error) => {
            rollback_unpublished(client, stream)?;
            return Err(error);
        }
    };
    if let Err(error) = require_not_cancelled(cancellation, "start capture generation") {
        rollback_unpublished(client, stream)?;
        return Err(error);
    }
    let remote = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            rollback_unpublished(client, stream)?;
            return Err(KernelDisplayError::new(
                "start capture generation",
                "operation was cancelled",
            ));
        }
        result = config.producer_remotes.create_producer_remote() => result.map_err(|error| {
            KernelDisplayError::new("connect classified PipeWire producer", error.to_string())
        }),
    };
    let remote = match remote {
        Ok(remote) => remote,
        Err(error) => {
            rollback_unpublished(client, stream)?;
            return Err(error);
        }
    };

    let node_name = format!("pronk.video.{}.{}", config.session_id, media_generation);
    let source_config = VideoSourceConfig {
        node_name,
        node_description: config.node_description.clone(),
        session_id: config.session_id.clone(),
        device_instance: config.device_instance.clone(),
        connector_id,
        output_index: config.output_index,
        grant_id,
        media_generation,
        refresh_hz: stream.refresh_hz,
    };
    let video_buffers = match export_video_buffers(client, stream, &buffers) {
        Ok(buffers) => buffers,
        Err(error) => {
            rollback_unpublished(client, stream)?;
            return Err(error);
        }
    };
    let identity = match video
        .start(VideoSourceGeneration {
            config: source_config,
            buffers: video_buffers,
            remote: remote.into_remote(),
        })
        .await
        .map_err(|error| KernelDisplayError::new("start PipeWire source", error.to_string()))
    {
        Ok(identity) => identity,
        Err(error) => {
            rollback_unpublished(client, stream)?;
            return Err(error);
        }
    };
    if cancellation.is_cancelled() {
        stop_started_source(video, media_generation).await?;
        rollback_unpublished(client, stream)?;
        return Err(KernelDisplayError::new(
            "start PipeWire source",
            "operation was cancelled",
        ));
    }
    if identity.media_generation != media_generation {
        stop_started_source(video, media_generation).await?;
        rollback_unpublished(client, stream)?;
        return Err(KernelDisplayError::new(
            "start PipeWire source",
            "source returned a stale media generation",
        ));
    }

    let target = DeviceMediaTarget {
        kind: DeviceMediaKind::Video,
        node_name: identity.node_name.clone(),
        object_serial: identity.object_serial,
        session_id: config.session_id.clone(),
        device_instance: config.device_instance.clone(),
        connector_id,
        output_index: config.output_index,
        media_generation,
        caps: raw_video_caps(stream),
    };
    let audio_target = audio_sink.map(|identity| DeviceMediaTarget {
        kind: DeviceMediaKind::Audio,
        node_name: identity.node_name,
        object_serial: identity.object_serial,
        session_id: config.session_id.clone(),
        device_instance: config.device_instance.clone(),
        connector_id,
        output_index: config.output_index,
        media_generation,
        caps: raw_audio_caps().into(),
    });
    let prepared = PreparedCaptureMedia {
        media_generation,
        video_target: target,
        audio_target,
        configuration: DeviceMediaConfiguration {
            video_profile_id: config.video_profile_id.clone(),
            audio_profile_id: config.audio_profile_id.clone(),
            mode: request.route.mode,
            video_bitrate: config.video_bitrate,
        },
    };
    Ok((
        ActiveGeneration {
            media_generation,
            stream,
            identity,
            transports: HashMap::with_capacity(CAPTURE_POOL_SIZE),
            available: VecDeque::with_capacity(CAPTURE_POOL_SIZE),
            outstanding: VecDeque::with_capacity(MAX_OUTSTANDING_CAPTURE_REQUESTS),
            next_user_data: 1,
            source_active: true,
            // The PipeWire target must exist and negotiate before the backend
            // completes its Cast handshake, but publishing frames during that
            // handshake leaves stale buffers queued at pipewiresrc. GstBaseSrc
            // then anchors its live clock to the stale first frame and paces
            // fresh frames by the handshake gap. The media driver activates
            // capture immediately before asking the configured graph to play.
            suspended: true,
        },
        prepared,
    ))
}

fn raw_audio_caps() -> &'static str {
    "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2"
}

async fn stop_started_source(
    video: &VideoSourceActor,
    media_generation: NonZeroU64,
) -> Result<(), KernelDisplayError> {
    match video.stop(media_generation).await {
        Ok(_)
        | Err(VideoSourceActorError::Shutdown { .. })
        | Err(VideoSourceActorError::NoActiveGeneration) => Ok(()),
        Err(error) => Err(KernelDisplayError::new(
            "roll back PipeWire source",
            error.to_string(),
        )),
    }
}

fn allocate_pool(
    client: &mut AsyncCastKmsClient,
    stream: CaptureStreamInfo,
) -> Result<Vec<CaptureBufferInfo>, KernelDisplayError> {
    let mut buffers = Vec::with_capacity(CAPTURE_POOL_SIZE);
    for index in 0..CAPTURE_POOL_SIZE {
        let buffer = client
            .client_mut()
            .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Explicit)
            .map_err(|error| {
                KernelDisplayError::new(
                    "allocate CastKMS capture pool",
                    format!("buffer {index}: {error}"),
                )
            })?;
        if buffer.stream_id != stream.stream_id {
            return Err(KernelDisplayError::new(
                "allocate CastKMS capture pool",
                "buffer belongs to another stream",
            ));
        }
        buffers.push(buffer);
    }
    Ok(buffers)
}

fn export_video_buffers(
    client: &AsyncCastKmsClient,
    stream: CaptureStreamInfo,
    buffers: &[CaptureBufferInfo],
) -> Result<Vec<VideoBuffer>, KernelDisplayError> {
    buffers
        .iter()
        .map(|buffer| {
            let exported = client
                .client()
                .export_capture_buffer(stream.stream_id, buffer.buffer_id)
                .map_err(|error| {
                    KernelDisplayError::new("export CastKMS capture buffer", error.to_string())
                })?;
            let timelines = exported.timelines.ok_or_else(|| {
                KernelDisplayError::new(
                    "export CastKMS capture buffer",
                    "explicit buffer has no sync timelines",
                )
            })?;
            Ok(VideoBuffer {
                id: exported.buffer_id,
                dma_buf: exported.dma_buf,
                layout: VideoBufferLayout {
                    width: exported.layout.width,
                    height: exported.layout.height,
                    pitch: exported.layout.pitch,
                    size: exported.layout.size,
                    modifier: exported.layout.modifier,
                },
                timelines: Some(VideoSyncTimelines {
                    ready: timelines.ready,
                    reuse: timelines.reuse,
                }),
            })
        })
        .collect()
}

fn validate_stream_route(
    stream: CaptureStreamInfo,
    request: MediaStartRequest,
) -> Result<(), KernelDisplayError> {
    if stream.crtc_id != request.route.target.as_nonzero()
        || stream.width.get() != request.route.mode.width
        || stream.height.get() != request.route.mode.height
    {
        return Err(KernelDisplayError::new(
            "validate CastKMS capture route",
            format!(
                "kernel stream is CRTC {} at {}x{}; route is CRTC {} at {}x{}",
                stream.crtc_id,
                stream.width,
                stream.height,
                request.route.target.get(),
                request.route.mode.width,
                request.route.mode.height,
            ),
        ));
    }
    Ok(())
}

fn raw_video_caps(stream: CaptureStreamInfo) -> String {
    format!(
        "video/x-raw,format=BGRx,width={},height={},framerate={}/1",
        stream.width, stream.height, stream.refresh_hz
    )
}

fn handle_source_event(
    client: &mut AsyncCastKmsClient,
    active: Option<&mut ActiveGeneration>,
    event: VideoSourceActorEvent,
) -> Result<(), KernelDisplayError> {
    let active = active.ok_or_else(|| {
        KernelDisplayError::new("handle PipeWire source event", "no active generation")
    })?;
    let event_generation = source_event_generation(&event);
    if !source_generation_is_current(active.media_generation, event_generation)? {
        // Stop joins the old source before its CastKMS pool is retired, but
        // events already published to this actor's bounded queue can outlive
        // that stop reply. Their immutable generation makes them safe to
        // discard instead of poisoning a newer generation.
        return Ok(());
    }
    match event {
        VideoSourceActorEvent::BufferAvailable {
            media_generation: _,
            buffer_id,
            transport,
        } => {
            if active.transports.contains_key(&buffer_id) {
                return Err(KernelDisplayError::new(
                    "handle PipeWire source event",
                    format!("buffer {buffer_id} negotiated its transport twice"),
                ));
            }
            if active.available.contains(&buffer_id) {
                return Err(KernelDisplayError::new(
                    "handle PipeWire source event",
                    format!("buffer {buffer_id} became available twice"),
                ));
            }
            active.transports.insert(buffer_id, transport);
            active.available.push_back(buffer_id);
        }
        VideoSourceActorEvent::BufferReleased {
            media_generation: _,
            buffer_id,
        } => {
            if !active.transports.contains_key(&buffer_id) {
                return Err(KernelDisplayError::new(
                    "handle PipeWire source event",
                    format!("buffer {buffer_id} was released before transport negotiation"),
                ));
            }
            client
                .client_mut()
                .release_capture_buffer(active.stream.stream_id, buffer_id)
                .map_err(|error| {
                    KernelDisplayError::new("release CastKMS capture buffer", error.to_string())
                })?;
            active.available.push_back(buffer_id);
        }
        VideoSourceActorEvent::GenerationFailed {
            identity, error, ..
        } => {
            debug_assert_eq!(identity.media_generation, active.media_generation);
            active.source_active = false;
            return Err(KernelDisplayError::new(
                "run PipeWire source generation",
                error.to_string(),
            ));
        }
    }
    Ok(())
}

fn source_event_generation(event: &VideoSourceActorEvent) -> NonZeroU64 {
    match event {
        VideoSourceActorEvent::BufferAvailable {
            media_generation, ..
        }
        | VideoSourceActorEvent::BufferReleased {
            media_generation, ..
        } => *media_generation,
        VideoSourceActorEvent::GenerationFailed { identity, .. } => identity.media_generation,
    }
}

fn capture_queue_needs_retry(active: Option<&ActiveGeneration>) -> bool {
    active.is_some_and(|active| {
        !active.suspended
            && active.outstanding.len() < MAX_OUTSTANDING_CAPTURE_REQUESTS
            && !active.available.is_empty()
    })
}

fn capture_queue_matches_frame(queue: &CaptureQueue, frame: &CaptureFrameEvent) -> bool {
    frame.stream_id == queue.stream_id.get()
        && frame.buffer_id == queue.buffer_id.get()
        && frame.user_data == queue.user_data.get()
}

fn maybe_queue(
    client: &mut AsyncCastKmsClient,
    active: Option<&mut ActiveGeneration>,
) -> Result<(), QueueFailure> {
    let Some(active) = active else {
        return Ok(());
    };
    if active.suspended || active.outstanding.len() >= MAX_OUTSTANDING_CAPTURE_REQUESTS {
        return Ok(());
    }
    let Some(buffer_id) = active.available.pop_front() else {
        return Ok(());
    };
    let transport = active.transports.get(&buffer_id).copied().ok_or_else(|| {
        QueueFailure::Fatal(KernelDisplayError::new(
            "queue CastKMS capture buffer",
            format!("buffer {buffer_id} has no negotiated PipeWire transport"),
        ))
    })?;
    let user_data = NonZeroU64::new(active.next_user_data).ok_or_else(|| {
        QueueFailure::Recoverable(KernelDisplayError::new(
            "queue CastKMS capture buffer",
            "user-data counter overflowed",
        ))
    })?;
    let queue = match client
        .client_mut()
        .queue_capture_buffer(buffer_id, user_data)
    {
        Ok(queue) => queue,
        Err(CaptureError::QueueBuffer(Errno::EBUSY)) => {
            active.available.push_front(buffer_id);
            return Ok(());
        }
        Err(error) => {
            return Err(QueueFailure::Recoverable(KernelDisplayError::new(
                "queue CastKMS capture buffer",
                error.to_string(),
            )))
        }
    };
    active.next_user_data = active.next_user_data.checked_add(1).unwrap_or(0);
    let fence = client
        .client()
        .arm_explicit_capture_fence(active.stream.stream_id, buffer_id)
        .map_err(|error| {
            QueueFailure::Fatal(KernelDisplayError::new(
                "arm CastKMS capture fence",
                error.to_string(),
            ))
        })?;
    active.outstanding.push_back(OutstandingCapture {
        queue,
        fence,
        transport,
    });
    Ok(())
}

async fn handle_capture_frame(
    client: &mut AsyncCastKmsClient,
    video: &VideoSourceActor,
    active: Option<&mut ActiveGeneration>,
    frame: CaptureFrameEvent,
) -> Result<(), KernelDisplayError> {
    let active = active.ok_or_else(|| {
        KernelDisplayError::new(
            "handle CastKMS capture frame",
            "frame arrived without an active media generation",
        )
    })?;
    let outstanding_index = active
        .outstanding
        .iter()
        .position(|capture| capture_queue_matches_frame(&capture.queue, &frame))
        .ok_or_else(|| {
            KernelDisplayError::new(
                "handle CastKMS capture frame",
                "frame identity differs from every outstanding buffer",
            )
        })?;
    let outstanding = &active.outstanding[outstanding_index];
    if frame.status != 0 || frame.mode_generation != active.stream.mode_generation.get() {
        return Err(KernelDisplayError::new(
            "handle CastKMS capture frame",
            format!(
                "capture completed with status {} at mode generation {}",
                frame.status, frame.mode_generation
            ),
        ));
    }
    let damage_x = u32::try_from(frame.damage_x).map_err(|_| {
        KernelDisplayError::new("publish captured frame", "negative damage x coordinate")
    })?;
    let damage_y = u32::try_from(frame.damage_y).map_err(|_| {
        KernelDisplayError::new("publish captured frame", "negative damage y coordinate")
    })?;
    let damage_width = NonZeroU32::new(frame.damage_width)
        .ok_or_else(|| KernelDisplayError::new("publish captured frame", "zero damage width"))?;
    let damage_height = NonZeroU32::new(frame.damage_height)
        .ok_or_else(|| KernelDisplayError::new("publish captured frame", "zero damage height"))?;
    if outstanding.transport == PipeWireBufferTransport::Waited {
        timeout(CAPTURE_DRAIN_TIMEOUT, outstanding.fence.wait_ready())
            .await
            .map_err(|_| {
                KernelDisplayError::new(
                    "wait for CastKMS capture readiness",
                    "timed out before publishing a waited PipeWire buffer",
                )
            })?
            .map_err(|error| {
                KernelDisplayError::new("wait for CastKMS capture readiness", error.to_string())
            })?;
    }
    let outstanding = active
        .outstanding
        .remove(outstanding_index)
        .expect("capture identity and readiness were validated above");
    let acquire_point = match outstanding.transport {
        PipeWireBufferTransport::Waited => None,
        PipeWireBufferTransport::SyncTimeline => outstanding.queue.ready_point,
    };
    let completion = client
        .client_mut()
        .delegate_explicit_capture_completion(outstanding.fence)
        .map_err(|error| {
            KernelDisplayError::new("delegate CastKMS capture completion", error.to_string())
        })?;
    video
        .publish(
            active.media_generation,
            VideoFrame {
                buffer_id: completion.queue.buffer_id,
                sequence: frame.sequence,
                pts_ns: frame.timestamp_ns,
                damage: VideoDamage {
                    x: damage_x,
                    y: damage_y,
                    width: damage_width,
                    height: damage_height,
                },
                discontinuity: frame.dropped_frames != 0,
                acquire_point,
            },
        )
        .await
        .map_err(|error| {
            KernelDisplayError::new("publish captured PipeWire frame", error.to_string())
        })?;
    Ok(())
}

async fn stop_active(
    client: &mut AsyncCastKmsClient,
    video: &VideoSourceActor,
    active: &mut Option<ActiveGeneration>,
) -> Result<(), KernelDisplayError> {
    let Some(mut generation) = active.take() else {
        return Ok(());
    };
    generation.suspended = true;
    let mut failures = Vec::new();
    if generation.source_active {
        match video.stop(generation.media_generation).await {
            Ok(report) => {
                if report.identity != generation.identity {
                    failures.push("PipeWire stop returned another generation identity".into());
                }
            }
            Err(VideoSourceActorError::Shutdown { report, source }) => {
                if report.identity != generation.identity {
                    failures.push("PipeWire failed stop returned another identity".into());
                }
                failures.push(format!("shut down PipeWire generation: {source}"));
            }
            // A runtime failure can quiesce the source actor and enqueue its
            // GenerationFailed event before this stop command wins the outer
            // select. CastKMS still has the authoritative buffer ownership
            // below, so that race is already stopped rather than erroneous.
            Err(VideoSourceActorError::NoActiveGeneration) => {}
            Err(error) => failures.push(format!("stop PipeWire generation: {error}")),
        }
        generation.source_active = false;
    }

    release_consumer_owned(client, generation.stream, &mut failures);
    if client.client().active_capture_stream() == Some(generation.stream) {
        if let Err(error) = client.client_mut().stop_capture() {
            failures.push(format!("stop CastKMS capture: {error}"));
        }
    }
    while let Some(outstanding) = generation.outstanding.pop_front() {
        if let Err(error) = drain_outstanding_capture(client, outstanding).await {
            failures.push(error.to_string());
        }
    }
    release_consumer_owned(client, generation.stream, &mut failures);
    if let Err(error) = client
        .client_mut()
        .finish_retired_capture(generation.stream.stream_id)
    {
        failures.push(format!("destroy retired CastKMS capture pool: {error}"));
    }
    combined_kernel_failures("stop capture generation", failures)
}

fn release_consumer_owned(
    client: &mut AsyncCastKmsClient,
    stream: CaptureStreamInfo,
    failures: &mut Vec<String>,
) {
    for buffer in client.client().capture_buffers(stream.stream_id) {
        if buffer.state == CaptureBufferState::ConsumerOwned {
            if let Err(error) = client
                .client_mut()
                .release_capture_buffer(stream.stream_id, buffer.buffer_id)
            {
                failures.push(format!(
                    "release consumer-owned CastKMS buffer {}: {error}",
                    buffer.buffer_id
                ));
            }
        }
    }
}

async fn drain_outstanding_capture(
    client: &mut AsyncCastKmsClient,
    outstanding: OutstandingCapture,
) -> Result<(), KernelDisplayError> {
    let deadline = tokio::time::Instant::now() + CAPTURE_DRAIN_TIMEOUT;
    loop {
        let state = client
            .client()
            .capture_buffers(outstanding.queue.stream_id)
            .into_iter()
            .find(|buffer| buffer.buffer_id == outstanding.queue.buffer_id)
            .map(|buffer| buffer.state)
            .ok_or_else(|| {
                KernelDisplayError::new(
                    "drain CastKMS capture",
                    "queued buffer disappeared during teardown",
                )
            })?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(KernelDisplayError::new(
                "drain CastKMS capture",
                "timed out waiting for canceled capture completion",
            ));
        }
        if state == CaptureBufferState::Completed {
            let ready = timeout(remaining, outstanding.fence.wait())
                .await
                .map_err(|_| {
                    KernelDisplayError::new(
                        "drain CastKMS capture",
                        "timed out waiting for capture-buffer readiness",
                    )
                })?
                .map_err(|error| {
                    KernelDisplayError::new(
                        "wait for drained CastKMS capture readiness",
                        error.to_string(),
                    )
                })?;
            let completion =
                client
                    .client_mut()
                    .take_capture_completion(ready)
                    .map_err(|error| {
                        KernelDisplayError::new(
                            "take drained CastKMS completion",
                            error.to_string(),
                        )
                    })?;
            client
                .client_mut()
                .release_capture_buffer(completion.queue.stream_id, completion.queue.buffer_id)
                .map_err(|error| {
                    KernelDisplayError::new("release drained CastKMS completion", error.to_string())
                })?;
            return Ok(());
        }
        if state != CaptureBufferState::Queued {
            return Err(KernelDisplayError::new(
                "drain CastKMS capture",
                format!("queued buffer entered unexpected state {state:?}"),
            ));
        }
        let events = timeout(remaining, client.read_events())
            .await
            .map_err(|_| {
                KernelDisplayError::new(
                    "drain CastKMS capture",
                    "timed out waiting for canceled capture event",
                )
            })?
            .map_err(|error| {
                KernelDisplayError::new("drain CastKMS event stream", error.to_string())
            })?;
        if events
            .iter()
            .any(|event| matches!(event, CastKmsEvent::GrantRevoked(_)))
        {
            // The frame cancellation still follows the terminal grant event;
            // continue draining until the exact buffer becomes Completed.
        }
    }
}

fn rollback_unpublished(
    client: &mut AsyncCastKmsClient,
    stream: CaptureStreamInfo,
) -> Result<(), KernelDisplayError> {
    if client.client().active_capture_stream() == Some(stream) {
        client.client_mut().stop_capture().map_err(|error| {
            KernelDisplayError::new("roll back CastKMS capture", error.to_string())
        })?;
    }
    if !client.client().capture_buffers(stream.stream_id).is_empty() {
        client
            .client_mut()
            .finish_retired_capture(stream.stream_id)
            .map_err(|error| {
                KernelDisplayError::new("destroy rolled-back capture pool", error.to_string())
            })?;
    }
    Ok(())
}

async fn fail_active(
    client: &mut AsyncCastKmsClient,
    video: &VideoSourceActor,
    active: &mut Option<ActiveGeneration>,
    events: &mpsc::UnboundedSender<KernelDisplayEvent>,
    error: String,
) -> Result<(), KernelDisplayError> {
    let cleanup = stop_active(client, video, active).await;
    let diagnostic = match &cleanup {
        Ok(()) => error,
        Err(cleanup) => format!("{error}; cleanup also failed: {cleanup}"),
    };
    let _ = events.send(KernelDisplayEvent::MediaFailed(diagnostic));
    cleanup.map_err(|cleanup| {
        KernelDisplayError::new(
            "recover failed media generation",
            format!("cleanup failed after media error: {cleanup}"),
        )
    })
}

fn publish_observation(
    events: &mpsc::UnboundedSender<KernelDisplayEvent>,
    current: &mut KernelDisplayObservation,
    observation: KernelDisplayObservation,
) {
    if observation != *current {
        *current = observation;
        let _ = events.send(KernelDisplayEvent::Changed(observation));
    }
}

fn require_generation(
    active: Option<&mut ActiveGeneration>,
    requested: NonZeroU64,
) -> Result<&mut ActiveGeneration, KernelDisplayError> {
    let active = active.ok_or_else(|| {
        KernelDisplayError::new("select capture generation", "no media generation is active")
    })?;
    if active.media_generation != requested {
        return Err(KernelDisplayError::new(
            "select capture generation",
            format!(
                "requested generation {requested}; active generation is {}",
                active.media_generation
            ),
        ));
    }
    Ok(active)
}

fn source_generation_is_current(
    active: NonZeroU64,
    requested: NonZeroU64,
) -> Result<bool, KernelDisplayError> {
    match requested.cmp(&active) {
        std::cmp::Ordering::Less => Ok(false),
        std::cmp::Ordering::Equal => Ok(true),
        std::cmp::Ordering::Greater => Err(KernelDisplayError::new(
            "handle PipeWire source event",
            format!("event generation {requested}; active generation is {active}"),
        )),
    }
}

fn require_not_cancelled(
    cancellation: &CancellationToken,
    operation: &'static str,
) -> Result<(), KernelDisplayError> {
    if cancellation.is_cancelled() {
        Err(KernelDisplayError::new(
            operation,
            "operation was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn combined_kernel_failures(
    operation: &'static str,
    failures: Vec<String>,
) -> Result<(), KernelDisplayError> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(KernelDisplayError::new(operation, failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_state::{MediaState, RouteTarget, RoutedMode};
    use crate::media_session::MediaRoute;

    fn request() -> MediaStartRequest {
        MediaStartRequest {
            media_generation: 9,
            route: MediaRoute {
                route_generation: 3,
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

    fn stream(width: u32) -> CaptureStreamInfo {
        CaptureStreamInfo {
            stream_id: NonZeroU32::new(2).unwrap(),
            crtc_id: NonZeroU32::new(7).unwrap(),
            mode_generation: NonZeroU64::new(11).unwrap(),
            width: NonZeroU32::new(width).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            refresh_hz: NonZeroU32::new(60).unwrap(),
            cursor_mode: CursorCaptureMode::IncludeInFrame,
        }
    }

    #[test]
    fn route_validation_keeps_kernel_and_application_generations_distinct() {
        validate_stream_route(stream(1920), request()).unwrap();
        assert!(validate_stream_route(stream(1280), request()).is_err());
        assert_eq!(
            raw_video_caps(stream(1920)),
            "video/x-raw,format=BGRx,width=1920,height=1080,framerate=60/1"
        );
    }

    #[test]
    fn asynchronous_media_failure_is_an_explicit_kernel_port_event() {
        let event = KernelDisplayEvent::MediaFailed("source disappeared".into());
        assert_eq!(
            event,
            KernelDisplayEvent::MediaFailed("source disappeared".into())
        );
        let _ = MediaState::Failed;
    }

    #[test]
    fn queued_source_events_are_filtered_only_when_they_are_stale() {
        let active = NonZeroU64::new(2).unwrap();
        assert!(!source_generation_is_current(active, NonZeroU64::new(1).unwrap()).unwrap());
        assert!(source_generation_is_current(active, active).unwrap());
        assert!(source_generation_is_current(active, NonZeroU64::new(3).unwrap()).is_err());
    }
}
