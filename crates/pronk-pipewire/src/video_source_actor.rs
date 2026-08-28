//! Generation-safe Tokio ownership of reusable PipeWire video sources.

use std::collections::HashMap;
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{
    PipeWireBufferTransport, PipeWireRemote, VideoBuffer, VideoFrame, VideoNodeIdentity,
    VideoSource, VideoSourceConfig, VideoSourceError, VideoSourceEvent, VideoSourceRuntimeError,
    MAX_VIDEO_BUFFERS,
};

pub const DEFAULT_VIDEO_SOURCE_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const ACTOR_COMMAND_CAPACITY: usize = 8;
const ACTOR_EVENT_CAPACITY: usize = MAX_VIDEO_BUFFERS * 2 + 8;
// A non-driving consumer may push RequestProcess, which the runtime handles
// immediately. Stock pipewiresrc instead only queues a shared buffer when its
// final GStreamer reference dies. While ownership remains submitted, use a
// quarter-frame deadline to guarantee a normal driver cycle without turning
// this into an always-running poll. Bounds keep unusual modes from creating
// either a hot loop or visible buffer-reuse latency.
const RETURN_TRIGGER_DIVISOR: u64 = 4;
const MIN_RETURN_TRIGGER_INTERVAL: Duration = Duration::from_millis(2);
const MAX_RETURN_TRIGGER_INTERVAL: Duration = Duration::from_millis(10);

type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything needed to start one immutable media generation.
#[derive(Debug)]
pub struct VideoSourceGeneration {
    pub config: VideoSourceConfig,
    pub buffers: Vec<VideoBuffer>,
    pub remote: PipeWireRemote,
}

/// Result of quiescing and joining one source generation.
///
/// `reclaimed_buffers` crossed the source handoff boundary before teardown;
/// this conservatively includes a handoff whose acknowledgement was lost. The
/// source loop is joined before this report is returned, so the CastKMS owner
/// may now release those buffers and retire their pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSourceStopReport {
    pub identity: VideoNodeIdentity,
    pub reclaimed_buffers: Box<[NonZeroU32]>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VideoSourceActorRuntimeError {
    #[error(transparent)]
    Source(#[from] VideoSourceRuntimeError),
    #[error(
        "PipeWire source emitted {event} for buffer {buffer_id} in an invalid ownership state"
    )]
    InvalidBufferEvent { event: &'static str, buffer_id: u32 },
    #[error("PipeWire source event stream closed unexpectedly")]
    EventStreamClosed,
    #[error("PipeWire source stopped unexpectedly")]
    UnexpectedStop,
    #[error("trigger PipeWire buffer-return processing: {0}")]
    ProcessTrigger(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSourceActorEvent {
    BufferAvailable {
        media_generation: NonZeroU64,
        buffer_id: NonZeroU32,
        transport: PipeWireBufferTransport,
    },
    BufferReleased {
        media_generation: NonZeroU64,
        buffer_id: NonZeroU32,
    },
    GenerationFailed {
        identity: VideoNodeIdentity,
        error: VideoSourceActorRuntimeError,
        reclaimed_buffers: Box<[NonZeroU32]>,
    },
}

#[derive(Debug, Error)]
pub enum VideoSourceActorError {
    #[error("VideoSourceActor requires a running Tokio runtime")]
    NoRuntime,
    #[error("video source actor command channel closed")]
    CommandClosed,
    #[error("video source actor reply channel closed")]
    ReplyClosed,
    #[error("video source actor task failed: {0}")]
    Join(String),
    #[error("media generation {active} is already active; cannot start {requested}")]
    GenerationActive { active: u64, requested: u64 },
    #[error("there is no active media generation")]
    NoActiveGeneration,
    #[error("media generation {requested} does not match active generation {active}")]
    GenerationMismatch { active: u64, requested: u64 },
    #[error("media generation {requested} is not newer than completed generation {previous}")]
    NonMonotonicGeneration { previous: u64, requested: u64 },
    #[error("replacement generation reused node name {0}")]
    ReusedNodeName(String),
    #[error("replacement generation reused PipeWire object serial {0}")]
    ReusedObjectSerial(u64),
    #[error("buffer {buffer_id} is not available in media generation {media_generation}")]
    BufferUnavailable {
        media_generation: u64,
        buffer_id: u32,
    },
    #[error(transparent)]
    Source(#[from] VideoSourceError),
    #[error("failed to stop PipeWire source after joining its loop: {source}")]
    Shutdown {
        report: Box<VideoSourceStopReport>,
        #[source]
        source: VideoSourceError,
    },
}

/// Bounded Tokio command/event bridge for one display's PipeWire source.
///
/// The actor permits at most one active generation. Call `stop()` and process
/// its reclaim report before destroying the old CastKMS pool, then call
/// `start()` with a strictly newer generation.
pub struct VideoSourceActor {
    commands: mpsc::Sender<ActorCommand>,
    events: mpsc::Receiver<VideoSourceActorEvent>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for VideoSourceActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoSourceActor")
            .finish_non_exhaustive()
    }
}

impl VideoSourceActor {
    pub fn spawn() -> Result<Self, VideoSourceActorError> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|_| VideoSourceActorError::NoRuntime)?;
        Ok(spawn_with_factory(
            &handle,
            RealSourceFactory {
                startup_timeout: DEFAULT_VIDEO_SOURCE_STARTUP_TIMEOUT,
            },
        ))
    }

    pub async fn start(
        &self,
        generation: VideoSourceGeneration,
    ) -> Result<VideoNodeIdentity, VideoSourceActorError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Start { generation, reply })
            .await
            .map_err(|_| VideoSourceActorError::CommandClosed)?;
        response
            .await
            .map_err(|_| VideoSourceActorError::ReplyClosed)?
    }

    pub async fn publish(
        &self,
        media_generation: NonZeroU64,
        frame: VideoFrame,
    ) -> Result<(), VideoSourceActorError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Publish {
                media_generation,
                frame,
                reply,
            })
            .await
            .map_err(|_| VideoSourceActorError::CommandClosed)?;
        response
            .await
            .map_err(|_| VideoSourceActorError::ReplyClosed)?
    }

    pub async fn stop(
        &self,
        media_generation: NonZeroU64,
    ) -> Result<VideoSourceStopReport, VideoSourceActorError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Stop {
                media_generation,
                reply,
            })
            .await
            .map_err(|_| VideoSourceActorError::CommandClosed)?;
        response
            .await
            .map_err(|_| VideoSourceActorError::ReplyClosed)?
    }

    pub async fn next_event(&mut self) -> Option<VideoSourceActorEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(
        mut self,
    ) -> Result<Option<VideoSourceStopReport>, VideoSourceActorError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Shutdown { reply: Some(reply) })
            .await
            .map_err(|_| VideoSourceActorError::CommandClosed)?;
        let result = response
            .await
            .map_err(|_| VideoSourceActorError::ReplyClosed)?;
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| VideoSourceActorError::Join(error.to_string()))?;
        }
        result
    }
}

impl Drop for VideoSourceActor {
    fn drop(&mut self) {
        let _ = self
            .commands
            .try_send(ActorCommand::Shutdown { reply: None });
        if let Some(task) = self.task.take() {
            // `shutdown` is the ordered buffer-reclamation path. Unexpected
            // owner loss must not detach this resource-owning task, especially
            // when a full command queue made the best-effort request fail.
            task.abort();
        }
    }
}

enum ActorCommand {
    Start {
        generation: VideoSourceGeneration,
        reply: oneshot::Sender<Result<VideoNodeIdentity, VideoSourceActorError>>,
    },
    Publish {
        media_generation: NonZeroU64,
        frame: VideoFrame,
        reply: oneshot::Sender<Result<(), VideoSourceActorError>>,
    },
    Stop {
        media_generation: NonZeroU64,
        reply: oneshot::Sender<Result<VideoSourceStopReport, VideoSourceActorError>>,
    },
    Shutdown {
        reply:
            Option<oneshot::Sender<Result<Option<VideoSourceStopReport>, VideoSourceActorError>>>,
    },
}

trait ManagedSource: Send + 'static {
    fn identity(&self) -> &VideoNodeIdentity;
    fn publish(&self, frame: VideoFrame) -> SourceFuture<'_, Result<(), VideoSourceError>>;
    fn trigger_process(&self) -> SourceFuture<'_, Result<(), VideoSourceError>>;
    fn next_event(&mut self) -> SourceFuture<'_, Option<VideoSourceEvent>>;
    fn shutdown(self) -> SourceFuture<'static, Result<(), VideoSourceError>>;
}

trait SourceFactory: Send + 'static {
    type Source: ManagedSource;

    fn start(
        &mut self,
        generation: VideoSourceGeneration,
    ) -> SourceFuture<'static, Result<Self::Source, VideoSourceError>>;
}

struct RealSourceFactory {
    startup_timeout: Duration,
}

impl SourceFactory for RealSourceFactory {
    type Source = VideoSource;

    fn start(
        &mut self,
        generation: VideoSourceGeneration,
    ) -> SourceFuture<'static, Result<Self::Source, VideoSourceError>> {
        let timeout = self.startup_timeout;
        Box::pin(VideoSource::start_with_timeout(
            generation.config,
            generation.buffers,
            generation.remote,
            timeout,
        ))
    }
}

impl ManagedSource for VideoSource {
    fn identity(&self) -> &VideoNodeIdentity {
        VideoSource::identity(self)
    }

    fn publish(&self, frame: VideoFrame) -> SourceFuture<'_, Result<(), VideoSourceError>> {
        Box::pin(VideoSource::publish(self, frame))
    }

    fn trigger_process(&self) -> SourceFuture<'_, Result<(), VideoSourceError>> {
        Box::pin(VideoSource::trigger_process(self))
    }

    fn next_event(&mut self) -> SourceFuture<'_, Option<VideoSourceEvent>> {
        Box::pin(VideoSource::next_event(self))
    }

    fn shutdown(self) -> SourceFuture<'static, Result<(), VideoSourceError>> {
        Box::pin(VideoSource::shutdown(self))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorBufferState {
    AwaitingInitial,
    Available,
    Submitted,
}

struct ActiveGeneration<S> {
    source: S,
    identity: VideoNodeIdentity,
    buffers: HashMap<NonZeroU32, ActorBufferState>,
    return_trigger_interval: Duration,
    return_trigger_deadline: Option<Instant>,
}

impl<S> ActiveGeneration<S> {
    fn new(
        source: S,
        identity: VideoNodeIdentity,
        buffer_ids: Vec<NonZeroU32>,
        return_trigger_interval: Duration,
    ) -> Self {
        Self {
            source,
            identity,
            buffers: buffer_ids
                .into_iter()
                .map(|id| (id, ActorBufferState::AwaitingInitial))
                .collect(),
            return_trigger_interval,
            return_trigger_deadline: None,
        }
    }

    fn has_submitted_buffers(&self) -> bool {
        self.buffers
            .values()
            .any(|state| *state == ActorBufferState::Submitted)
    }

    fn arm_return_trigger(&mut self) {
        if self.return_trigger_deadline.is_none() && self.has_submitted_buffers() {
            self.return_trigger_deadline = Some(Instant::now() + self.return_trigger_interval);
        }
    }

    fn rearm_return_trigger(&mut self) {
        self.return_trigger_deadline = self
            .has_submitted_buffers()
            .then(|| Instant::now() + self.return_trigger_interval);
    }

    fn disarm_return_trigger_if_idle(&mut self) {
        if !self.has_submitted_buffers() {
            self.return_trigger_deadline = None;
        }
    }

    fn stop_report(&self) -> VideoSourceStopReport {
        let mut reclaimed_buffers = self
            .buffers
            .iter()
            .filter_map(|(id, state)| (*state == ActorBufferState::Submitted).then_some(*id))
            .collect::<Vec<_>>();
        reclaimed_buffers.sort_unstable();
        VideoSourceStopReport {
            identity: self.identity.clone(),
            reclaimed_buffers: reclaimed_buffers.into_boxed_slice(),
        }
    }
}

#[derive(Default)]
struct ActorHistory {
    generation: Option<NonZeroU64>,
    node_name: Option<String>,
    object_serial: Option<NonZeroU64>,
}

enum ActorInput {
    Command(Option<ActorCommand>),
    SourceEvent(Option<VideoSourceEvent>),
    ReturnTrigger,
}

fn spawn_with_factory<F>(handle: &tokio::runtime::Handle, factory: F) -> VideoSourceActor
where
    F: SourceFactory,
{
    let (commands, command_receiver) = mpsc::channel(ACTOR_COMMAND_CAPACITY);
    let (event_sender, events) = mpsc::channel(ACTOR_EVENT_CAPACITY);
    let task = handle.spawn(run_actor(factory, command_receiver, event_sender));
    VideoSourceActor {
        commands,
        events,
        task: Some(task),
    }
}

async fn run_actor<F>(
    mut factory: F,
    mut commands: mpsc::Receiver<ActorCommand>,
    events: mpsc::Sender<VideoSourceActorEvent>,
) where
    F: SourceFactory,
{
    let mut active: Option<ActiveGeneration<F::Source>> = None;
    let mut history = ActorHistory::default();

    loop {
        let input = match active.as_mut() {
            Some(active) => match active.return_trigger_deadline {
                Some(deadline) => tokio::select! {
                    command = commands.recv() => ActorInput::Command(command),
                    event = active.source.next_event() => ActorInput::SourceEvent(event),
                    _ = tokio::time::sleep_until(deadline) => ActorInput::ReturnTrigger,
                },
                None => tokio::select! {
                    command = commands.recv() => ActorInput::Command(command),
                    event = active.source.next_event() => ActorInput::SourceEvent(event),
                },
            },
            None => ActorInput::Command(commands.recv().await),
        };

        match input {
            ActorInput::Command(Some(ActorCommand::Start { generation, reply })) => {
                let requested = generation.config.media_generation;
                if let Some(current) = active.as_ref() {
                    let _ = reply.send(Err(VideoSourceActorError::GenerationActive {
                        active: current.identity.media_generation.get(),
                        requested: requested.get(),
                    }));
                    continue;
                }
                match start_generation(&mut factory, generation, &history).await {
                    Ok(started) => {
                        let identity = started.identity.clone();
                        history.generation = Some(identity.media_generation);
                        history.node_name = Some(identity.node_name.clone());
                        history.object_serial = Some(identity.object_serial);
                        active = Some(started);
                        let _ = reply.send(Ok(identity));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            ActorInput::Command(Some(ActorCommand::Publish {
                media_generation,
                frame,
                reply,
            })) => {
                let result = match active.as_mut() {
                    Some(current) if current.identity.media_generation == media_generation => {
                        publish_frame(current, frame).await
                    }
                    Some(current) => Err(VideoSourceActorError::GenerationMismatch {
                        active: current.identity.media_generation.get(),
                        requested: media_generation.get(),
                    }),
                    None => Err(VideoSourceActorError::NoActiveGeneration),
                };
                let _ = reply.send(result);
            }
            ActorInput::Command(Some(ActorCommand::Stop {
                media_generation,
                reply,
            })) => {
                let active_generation = active
                    .as_ref()
                    .map(|current| current.identity.media_generation);
                let result = match active_generation {
                    Some(current) if current != media_generation => {
                        Err(VideoSourceActorError::GenerationMismatch {
                            active: current.get(),
                            requested: media_generation.get(),
                        })
                    }
                    Some(_) => {
                        let current = active.take().expect("active generation checked");
                        stop_generation(current).await
                    }
                    None => Err(VideoSourceActorError::NoActiveGeneration),
                };
                let _ = reply.send(result);
            }
            ActorInput::Command(Some(ActorCommand::Shutdown { reply })) => {
                let result = match active.take() {
                    Some(current) => stop_generation(current).await.map(Some),
                    None => Ok(None),
                };
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                break;
            }
            ActorInput::Command(None) => {
                if let Some(current) = active.take() {
                    let _ = stop_generation(current).await;
                }
                break;
            }
            ActorInput::SourceEvent(event) => {
                let result = handle_source_event(
                    active
                        .as_mut()
                        .expect("source events require an active generation"),
                    event,
                );
                match result {
                    Ok(Some(event)) => {
                        if events.send(event).await.is_err() {
                            if let Some(current) = active.take() {
                                let _ = stop_generation(current).await;
                            }
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if report_generation_failure(&mut active, &events, error).await {
                            break;
                        }
                    }
                }
            }
            ActorInput::ReturnTrigger => {
                let current = active
                    .as_mut()
                    .expect("return triggers require an active generation");
                match current.source.trigger_process().await {
                    Ok(()) => current.rearm_return_trigger(),
                    Err(error) => {
                        let error = VideoSourceActorRuntimeError::ProcessTrigger(error.to_string());
                        if report_generation_failure(&mut active, &events, error).await {
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn start_generation<F: SourceFactory>(
    factory: &mut F,
    generation: VideoSourceGeneration,
    history: &ActorHistory,
) -> Result<ActiveGeneration<F::Source>, VideoSourceActorError> {
    generation
        .config
        .validate(&generation.buffers)
        .map_err(VideoSourceError::from)?;
    let requested_generation = generation.config.media_generation;
    if let Some(previous) = history.generation {
        if requested_generation <= previous {
            return Err(VideoSourceActorError::NonMonotonicGeneration {
                previous: previous.get(),
                requested: requested_generation.get(),
            });
        }
    }
    if history.node_name.as_deref() == Some(generation.config.node_name.as_str()) {
        return Err(VideoSourceActorError::ReusedNodeName(
            generation.config.node_name,
        ));
    }
    let return_trigger_interval = return_trigger_interval(generation.config.refresh_hz);
    let buffer_ids = generation.buffers.iter().map(|buffer| buffer.id).collect();
    let source = factory.start(generation).await?;
    let identity = source.identity().clone();
    if history.object_serial == Some(identity.object_serial) {
        let serial = identity.object_serial.get();
        source.shutdown().await?;
        return Err(VideoSourceActorError::ReusedObjectSerial(serial));
    }
    Ok(ActiveGeneration::new(
        source,
        identity,
        buffer_ids,
        return_trigger_interval,
    ))
}

async fn publish_frame<S: ManagedSource>(
    active: &mut ActiveGeneration<S>,
    frame: VideoFrame,
) -> Result<(), VideoSourceActorError> {
    let state = active.buffers.get_mut(&frame.buffer_id).ok_or(
        VideoSourceActorError::BufferUnavailable {
            media_generation: active.identity.media_generation.get(),
            buffer_id: frame.buffer_id.get(),
        },
    )?;
    if *state != ActorBufferState::Available {
        return Err(VideoSourceActorError::BufferUnavailable {
            media_generation: active.identity.media_generation.get(),
            buffer_id: frame.buffer_id.get(),
        });
    }

    // Crossing the source call is an ownership handoff. Record it before
    // awaiting the acknowledgement so a closed reply channel cannot make a
    // possibly queued buffer look caller-owned in the stop report.
    *state = ActorBufferState::Submitted;
    active.arm_return_trigger();
    active.source.publish(frame).await?;
    Ok(())
}

fn handle_source_event<S>(
    active: &mut ActiveGeneration<S>,
    event: Option<VideoSourceEvent>,
) -> Result<Option<VideoSourceActorEvent>, VideoSourceActorRuntimeError> {
    let generation = active.identity.media_generation;
    match event {
        Some(VideoSourceEvent::BufferAvailable {
            buffer_id,
            transport,
        }) => {
            let state = active.buffers.get_mut(&buffer_id).ok_or(
                VideoSourceActorRuntimeError::InvalidBufferEvent {
                    event: "availability",
                    buffer_id: buffer_id.get(),
                },
            )?;
            if *state != ActorBufferState::AwaitingInitial {
                return Err(VideoSourceActorRuntimeError::InvalidBufferEvent {
                    event: "availability",
                    buffer_id: buffer_id.get(),
                });
            }
            *state = ActorBufferState::Available;
            Ok(Some(VideoSourceActorEvent::BufferAvailable {
                media_generation: generation,
                buffer_id,
                transport,
            }))
        }
        Some(VideoSourceEvent::BufferReleased { buffer_id }) => {
            let state = active.buffers.get_mut(&buffer_id).ok_or(
                VideoSourceActorRuntimeError::InvalidBufferEvent {
                    event: "release",
                    buffer_id: buffer_id.get(),
                },
            )?;
            if *state != ActorBufferState::Submitted {
                return Err(VideoSourceActorRuntimeError::InvalidBufferEvent {
                    event: "release",
                    buffer_id: buffer_id.get(),
                });
            }
            *state = ActorBufferState::Available;
            active.disarm_return_trigger_if_idle();
            Ok(Some(VideoSourceActorEvent::BufferReleased {
                media_generation: generation,
                buffer_id,
            }))
        }
        Some(VideoSourceEvent::Failed(error)) => Err(error.into()),
        Some(VideoSourceEvent::Stopped) => Err(VideoSourceActorRuntimeError::UnexpectedStop),
        None => Err(VideoSourceActorRuntimeError::EventStreamClosed),
    }
}

async fn report_generation_failure<S: ManagedSource>(
    active: &mut Option<ActiveGeneration<S>>,
    events: &mpsc::Sender<VideoSourceActorEvent>,
    error: VideoSourceActorRuntimeError,
) -> bool {
    let current = active.take().expect("failed source was active");
    let report = current.stop_report();
    let _ = current.source.shutdown().await;
    events
        .send(VideoSourceActorEvent::GenerationFailed {
            identity: report.identity,
            error,
            reclaimed_buffers: report.reclaimed_buffers,
        })
        .await
        .is_err()
}

fn return_trigger_interval(refresh_hz: NonZeroU32) -> Duration {
    let frame_ns = 1_000_000_000_u64.div_ceil(u64::from(refresh_hz.get()));
    Duration::from_nanos(frame_ns.div_ceil(RETURN_TRIGGER_DIVISOR))
        .clamp(MIN_RETURN_TRIGGER_INTERVAL, MAX_RETURN_TRIGGER_INTERVAL)
}

async fn stop_generation<S: ManagedSource>(
    active: ActiveGeneration<S>,
) -> Result<VideoSourceStopReport, VideoSourceActorError> {
    let report = active.stop_report();
    match active.source.shutdown().await {
        Ok(()) => Ok(report),
        Err(source) => Err(VideoSourceActorError::Shutdown {
            report: Box::new(report),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs::File;
    use std::sync::{Arc, Mutex};

    use tokio::runtime::Builder;

    use super::*;
    use crate::{VideoBufferLayout, VideoDamage, VideoSyncTimelines};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeRelease {
        OnPublish,
        OnTrigger,
        OnePerTrigger,
        PublishFailure,
        TriggerFailure,
        Never,
    }

    #[derive(Debug, Clone, Copy)]
    struct FakeSpec {
        object_id: u32,
        object_serial: u64,
        release: FakeRelease,
    }

    struct FakeFactory {
        specs: VecDeque<FakeSpec>,
        log: Arc<Mutex<Vec<String>>>,
        controls: Arc<Mutex<Vec<mpsc::Sender<VideoSourceEvent>>>>,
    }

    struct FakeHarness {
        factory: FakeFactory,
        log: Arc<Mutex<Vec<String>>>,
        controls: Arc<Mutex<Vec<mpsc::Sender<VideoSourceEvent>>>>,
    }

    struct FakeSource {
        identity: VideoNodeIdentity,
        events: mpsc::Receiver<VideoSourceEvent>,
        control: mpsc::Sender<VideoSourceEvent>,
        release: FakeRelease,
        pending_release: Mutex<Vec<NonZeroU32>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl SourceFactory for FakeFactory {
        type Source = FakeSource;

        fn start(
            &mut self,
            generation: VideoSourceGeneration,
        ) -> SourceFuture<'static, Result<Self::Source, VideoSourceError>> {
            let spec = self.specs.pop_front();
            let log = self.log.clone();
            let controls = self.controls.clone();
            Box::pin(async move {
                let spec = spec.ok_or_else(|| {
                    VideoSourceError::Runtime(VideoSourceRuntimeError::Stream(
                        "fake source specification is missing".to_string(),
                    ))
                })?;
                let (control, events) = mpsc::channel(16);
                for buffer in &generation.buffers {
                    control
                        .try_send(VideoSourceEvent::BufferAvailable {
                            buffer_id: buffer.id,
                            transport: PipeWireBufferTransport::SyncTimeline,
                        })
                        .expect("fake event queue has room");
                }
                controls
                    .lock()
                    .expect("fake controls mutex poisoned")
                    .push(control.clone());
                log.lock()
                    .expect("fake log mutex poisoned")
                    .push(format!("start:{}", generation.config.media_generation));
                Ok(FakeSource {
                    identity: VideoNodeIdentity {
                        node_name: generation.config.node_name,
                        object_id: nonzero32(spec.object_id),
                        object_serial: nonzero64(spec.object_serial),
                        media_generation: generation.config.media_generation,
                    },
                    events,
                    control,
                    release: spec.release,
                    pending_release: Mutex::new(Vec::new()),
                    log,
                })
            })
        }
    }

    impl ManagedSource for FakeSource {
        fn identity(&self) -> &VideoNodeIdentity {
            &self.identity
        }

        fn publish(&self, frame: VideoFrame) -> SourceFuture<'_, Result<(), VideoSourceError>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .expect("fake log mutex poisoned")
                    .push(format!(
                        "publish:{}:{}",
                        self.identity.media_generation, frame.buffer_id
                    ));
                match self.release {
                    FakeRelease::OnPublish => {
                        self.control
                            .send(VideoSourceEvent::BufferReleased {
                                buffer_id: frame.buffer_id,
                            })
                            .await
                            .map_err(|_| fake_source_closed())?;
                    }
                    FakeRelease::OnTrigger | FakeRelease::OnePerTrigger => {
                        self.pending_release
                            .lock()
                            .expect("fake pending-release mutex poisoned")
                            .push(frame.buffer_id);
                    }
                    FakeRelease::PublishFailure => {
                        return Err(VideoSourceError::Runtime(VideoSourceRuntimeError::Stream(
                            "fake publish acknowledgement failed".to_string(),
                        )));
                    }
                    FakeRelease::TriggerFailure | FakeRelease::Never => {}
                }
                Ok(())
            })
        }

        fn trigger_process(&self) -> SourceFuture<'_, Result<(), VideoSourceError>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .expect("fake log mutex poisoned")
                    .push(format!("trigger:{}", self.identity.media_generation));
                if self.release == FakeRelease::TriggerFailure {
                    return Err(VideoSourceError::Runtime(VideoSourceRuntimeError::Stream(
                        "fake process trigger failed".to_string(),
                    )));
                }
                let pending = {
                    let mut pending = self
                        .pending_release
                        .lock()
                        .expect("fake pending-release mutex poisoned");
                    if self.release == FakeRelease::OnePerTrigger && !pending.is_empty() {
                        vec![pending.remove(0)]
                    } else {
                        std::mem::take(&mut *pending)
                    }
                };
                for buffer_id in pending {
                    self.control
                        .send(VideoSourceEvent::BufferReleased { buffer_id })
                        .await
                        .map_err(|_| fake_source_closed())?;
                }
                Ok(())
            })
        }

        fn next_event(&mut self) -> SourceFuture<'_, Option<VideoSourceEvent>> {
            Box::pin(self.events.recv())
        }

        fn shutdown(self) -> SourceFuture<'static, Result<(), VideoSourceError>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .expect("fake log mutex poisoned")
                    .push(format!("shutdown:{}", self.identity.media_generation));
                Ok(())
            })
        }
    }

    #[test]
    fn actor_orders_generation_shutdown_before_replacement_and_rejects_stale_publish() {
        test_runtime().block_on(async {
            let FakeHarness {
                factory,
                log,
                controls: _,
            } = fake_factory([
                FakeSpec {
                    object_id: 10,
                    object_serial: 100,
                    release: FakeRelease::OnPublish,
                },
                FakeSpec {
                    object_id: 10,
                    object_serial: 101,
                    release: FakeRelease::OnPublish,
                },
            ]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            let first = actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            actor
                .publish(nonzero64(1), frame(nonzero32(1)))
                .await
                .unwrap();
            assert_eq!(
                actor.next_event().await,
                Some(VideoSourceActorEvent::BufferReleased {
                    media_generation: nonzero64(1),
                    buffer_id: nonzero32(1),
                })
            );
            assert!(actor
                .stop(nonzero64(1))
                .await
                .unwrap()
                .reclaimed_buffers
                .is_empty());

            let second = actor.start(generation(2)).await.unwrap();
            assert_ne!(first.object_serial, second.object_serial);
            drain_initial(&mut actor, nonzero64(2)).await;
            assert!(matches!(
                actor.publish(nonzero64(1), frame(nonzero32(1))).await,
                Err(VideoSourceActorError::GenerationMismatch {
                    active: 2,
                    requested: 1
                })
            ));
            actor.stop(nonzero64(2)).await.unwrap();
            assert!(actor.shutdown().await.unwrap().is_none());

            assert_eq!(
                *log.lock().unwrap(),
                [
                    "start:1",
                    "publish:1:1",
                    "shutdown:1",
                    "start:2",
                    "shutdown:2",
                ]
            );
        });
    }

    #[test]
    fn actor_triggers_normal_processing_until_shared_buffers_return() {
        test_runtime().block_on(async {
            let FakeHarness {
                factory,
                log,
                controls: _,
            } = fake_factory([FakeSpec {
                object_id: 10,
                object_serial: 100,
                release: FakeRelease::OnTrigger,
            }]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            for _ in 0..3 {
                actor
                    .publish(nonzero64(1), frame(nonzero32(1)))
                    .await
                    .unwrap();
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(1), actor.next_event())
                        .await
                        .expect("submitted buffer was not retriggered"),
                    Some(VideoSourceActorEvent::BufferReleased {
                        media_generation: nonzero64(1),
                        buffer_id: nonzero32(1),
                    })
                );
            }

            // Once the final release is observed, the deadline is disarmed
            // rather than becoming an idle periodic wakeup.
            tokio::time::sleep(MAX_RETURN_TRIGGER_INTERVAL * 2).await;
            actor.stop(nonzero64(1)).await.unwrap();
            actor.shutdown().await.unwrap();
            let log = log.lock().unwrap();
            assert_eq!(
                log.iter()
                    .filter(|entry| entry.starts_with("trigger:"))
                    .count(),
                3
            );
        });
    }

    #[test]
    fn return_trigger_interval_is_a_bounded_fraction_of_the_frame_period() {
        assert_eq!(
            return_trigger_interval(nonzero32(60)),
            Duration::from_nanos(4_166_667)
        );
        assert_eq!(
            return_trigger_interval(nonzero32(240)),
            MIN_RETURN_TRIGGER_INTERVAL
        );
        assert_eq!(
            return_trigger_interval(nonzero32(24)),
            MAX_RETURN_TRIGGER_INTERVAL
        );
    }

    #[test]
    fn actor_keeps_triggering_until_every_submitted_buffer_returns() {
        test_runtime().block_on(async {
            let FakeHarness { factory, log, .. } = fake_factory([FakeSpec {
                object_id: 10,
                object_serial: 100,
                release: FakeRelease::OnePerTrigger,
            }]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            for buffer_id in [nonzero32(1), nonzero32(2)] {
                actor.publish(nonzero64(1), frame(buffer_id)).await.unwrap();
            }

            for expected_buffer in [nonzero32(1), nonzero32(2)] {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(1), actor.next_event())
                        .await
                        .expect("submitted buffer was not retriggered"),
                    Some(VideoSourceActorEvent::BufferReleased {
                        media_generation: nonzero64(1),
                        buffer_id: expected_buffer,
                    })
                );
            }

            tokio::time::sleep(MAX_RETURN_TRIGGER_INTERVAL * 2).await;
            actor.stop(nonzero64(1)).await.unwrap();
            actor.shutdown().await.unwrap();
            let log = log.lock().unwrap();
            assert_eq!(
                log.iter()
                    .filter(|entry| entry.starts_with("trigger:"))
                    .count(),
                2
            );
        });
    }

    #[test]
    fn failed_return_trigger_fails_and_reclaims_the_generation() {
        test_runtime().block_on(async {
            let FakeHarness { factory, log, .. } = fake_factory([FakeSpec {
                object_id: 10,
                object_serial: 100,
                release: FakeRelease::TriggerFailure,
            }]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            actor
                .publish(nonzero64(1), frame(nonzero32(2)))
                .await
                .unwrap();
            match tokio::time::timeout(Duration::from_secs(1), actor.next_event())
                .await
                .expect("failed trigger did not terminate the generation")
                .expect("actor event stream closed")
            {
                VideoSourceActorEvent::GenerationFailed {
                    identity,
                    error: VideoSourceActorRuntimeError::ProcessTrigger(error),
                    reclaimed_buffers,
                } => {
                    assert_eq!(identity.media_generation, nonzero64(1));
                    assert!(error.contains("fake process trigger failed"));
                    assert_eq!(reclaimed_buffers.as_ref(), &[nonzero32(2)]);
                }
                event => panic!("unexpected actor event: {event:?}"),
            }
            assert!(actor.shutdown().await.unwrap().is_none());
            assert_eq!(
                *log.lock().unwrap(),
                ["start:1", "publish:1:2", "trigger:1", "shutdown:1"]
            );
        });
    }

    #[test]
    fn stop_reports_submitted_buffers_only_after_source_shutdown() {
        test_runtime().block_on(async {
            let FakeHarness {
                factory,
                log,
                controls: _,
            } = fake_factory([FakeSpec {
                object_id: 10,
                object_serial: 100,
                release: FakeRelease::Never,
            }]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            actor
                .publish(nonzero64(1), frame(nonzero32(2)))
                .await
                .unwrap();
            let report = actor.stop(nonzero64(1)).await.unwrap();
            assert_eq!(report.reclaimed_buffers.as_ref(), &[nonzero32(2)]);
            assert_eq!(
                *log.lock().unwrap(),
                ["start:1", "publish:1:2", "shutdown:1"]
            );
            actor.shutdown().await.unwrap();
        });
    }

    #[test]
    fn failed_publish_is_conservatively_reclaimed() {
        test_runtime().block_on(async {
            let FakeHarness { factory, log, .. } = fake_factory([FakeSpec {
                object_id: 10,
                object_serial: 100,
                release: FakeRelease::PublishFailure,
            }]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            assert!(matches!(
                actor.publish(nonzero64(1), frame(nonzero32(2))).await,
                Err(VideoSourceActorError::Source(VideoSourceError::Runtime(
                    VideoSourceRuntimeError::Stream(_)
                )))
            ));
            let report = actor.stop(nonzero64(1)).await.unwrap();
            assert_eq!(report.reclaimed_buffers.as_ref(), &[nonzero32(2)]);
            actor.shutdown().await.unwrap();
            assert_eq!(
                *log.lock().unwrap(),
                ["start:1", "publish:1:2", "shutdown:1"]
            );
        });
    }

    #[test]
    fn runtime_failure_reclaims_the_old_generation_and_drops_its_late_events() {
        test_runtime().block_on(async {
            let FakeHarness {
                factory,
                log,
                controls,
            } = fake_factory([
                FakeSpec {
                    object_id: 10,
                    object_serial: 100,
                    release: FakeRelease::Never,
                },
                FakeSpec {
                    object_id: 11,
                    object_serial: 101,
                    release: FakeRelease::OnPublish,
                },
            ]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            actor
                .publish(nonzero64(1), frame(nonzero32(1)))
                .await
                .unwrap();
            let control = controls.lock().unwrap()[0].clone();
            control
                .send(VideoSourceEvent::Failed(
                    VideoSourceRuntimeError::NodeRemoved,
                ))
                .await
                .unwrap();
            control
                .send(VideoSourceEvent::BufferReleased {
                    buffer_id: nonzero32(1),
                })
                .await
                .unwrap();

            match actor.next_event().await.unwrap() {
                VideoSourceActorEvent::GenerationFailed {
                    identity,
                    error,
                    reclaimed_buffers,
                } => {
                    assert_eq!(identity.media_generation, nonzero64(1));
                    assert_eq!(
                        error,
                        VideoSourceActorRuntimeError::Source(VideoSourceRuntimeError::NodeRemoved)
                    );
                    assert_eq!(reclaimed_buffers.as_ref(), &[nonzero32(1)]);
                }
                event => panic!("unexpected actor event: {event:?}"),
            }

            actor.start(generation(2)).await.unwrap();
            drain_initial(&mut actor, nonzero64(2)).await;
            actor.stop(nonzero64(2)).await.unwrap();
            actor.shutdown().await.unwrap();
            assert_eq!(
                *log.lock().unwrap(),
                [
                    "start:1",
                    "publish:1:1",
                    "shutdown:1",
                    "start:2",
                    "shutdown:2",
                ]
            );
        });
    }

    #[test]
    fn actor_rejects_nonmonotonic_generations_and_reused_object_serials() {
        test_runtime().block_on(async {
            let FakeHarness {
                factory,
                log,
                controls: _,
            } = fake_factory([
                FakeSpec {
                    object_id: 10,
                    object_serial: 100,
                    release: FakeRelease::OnPublish,
                },
                FakeSpec {
                    object_id: 11,
                    object_serial: 100,
                    release: FakeRelease::OnPublish,
                },
            ]);
            let handle = tokio::runtime::Handle::current();
            let mut actor = spawn_with_factory(&handle, factory);

            actor.start(generation(1)).await.unwrap();
            drain_initial(&mut actor, nonzero64(1)).await;
            actor.stop(nonzero64(1)).await.unwrap();
            assert!(matches!(
                actor.start(generation(1)).await,
                Err(VideoSourceActorError::NonMonotonicGeneration {
                    previous: 1,
                    requested: 1
                })
            ));
            assert!(matches!(
                actor.start(generation(2)).await,
                Err(VideoSourceActorError::ReusedObjectSerial(100))
            ));
            actor.shutdown().await.unwrap();
            assert_eq!(
                *log.lock().unwrap(),
                ["start:1", "shutdown:1", "start:2", "shutdown:2"]
            );
        });
    }

    #[test]
    fn dropping_the_owner_aborts_the_actor_instead_of_detaching_it() {
        test_runtime().block_on(async {
            let FakeHarness { factory, .. } = fake_factory([]);
            let handle = tokio::runtime::Handle::current();
            let actor = spawn_with_factory(&handle, factory);
            let task = actor
                .task
                .as_ref()
                .expect("spawned actor owns its task")
                .abort_handle();

            drop(actor);
            tokio::task::yield_now().await;
            assert!(task.is_finished());
        });
    }

    fn fake_factory<const N: usize>(specs: [FakeSpec; N]) -> FakeHarness {
        let log = Arc::new(Mutex::new(Vec::new()));
        let controls = Arc::new(Mutex::new(Vec::new()));
        FakeHarness {
            factory: FakeFactory {
                specs: specs.into(),
                log: log.clone(),
                controls: controls.clone(),
            },
            log,
            controls,
        }
    }

    fn fake_source_closed() -> VideoSourceError {
        VideoSourceError::Runtime(VideoSourceRuntimeError::Stream(
            "fake event receiver closed".to_string(),
        ))
    }

    async fn drain_initial(actor: &mut VideoSourceActor, generation: NonZeroU64) {
        for buffer_id in [nonzero32(1), nonzero32(2)] {
            assert_eq!(
                actor.next_event().await,
                Some(VideoSourceActorEvent::BufferAvailable {
                    media_generation: generation,
                    buffer_id,
                    transport: PipeWireBufferTransport::SyncTimeline,
                })
            );
        }
    }

    fn generation(value: u64) -> VideoSourceGeneration {
        let media_generation = nonzero64(value);
        VideoSourceGeneration {
            config: VideoSourceConfig {
                node_name: format!("pronk.test.generation-{media_generation}"),
                node_description: "Pronk actor test".to_string(),
                session_id: "test-session".to_string(),
                device_instance: "test-device".to_string(),
                connector_id: nonzero32(7),
                output_index: 0,
                grant_id: nonzero32(8),
                media_generation,
                refresh_hz: nonzero32(60),
            },
            buffers: vec![video_buffer(1), video_buffer(2)],
            remote: PipeWireRemote::AmbientDevelopment,
        }
    }

    fn video_buffer(id: u32) -> VideoBuffer {
        VideoBuffer {
            id: nonzero32(id),
            dma_buf: File::open("/dev/null").unwrap().into(),
            layout: VideoBufferLayout {
                width: nonzero32(640),
                height: nonzero32(480),
                pitch: nonzero32(2560),
                size: nonzero64(1_228_800),
                modifier: 0,
            },
            timelines: Some(VideoSyncTimelines {
                ready: File::open("/dev/null").unwrap().into(),
                reuse: File::open("/dev/null").unwrap().into(),
            }),
        }
    }

    fn frame(buffer_id: NonZeroU32) -> VideoFrame {
        VideoFrame {
            buffer_id,
            sequence: 1,
            pts_ns: 2,
            damage: VideoDamage {
                x: 0,
                y: 0,
                width: nonzero32(640),
                height: nonzero32(480),
            },
            discontinuity: false,
            acquire_point: Some(nonzero64(1)),
        }
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn nonzero32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nonzero64(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }
}
