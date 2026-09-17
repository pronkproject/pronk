use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use pronk_capture::{Actor, CaptureError};
use pronk_pipewire::{
    PipeWireRemote, VideoNodeIdentity, VideoSourceActor, VideoSourceActorEvent, VideoSourceConfig,
    VideoSourceGeneration,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{invalid, Output, Registration};

const COMMAND_CAPACITY: usize = 4;

/// State of the local capture/transport owner, not receiver playback or grant revocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    Stopped,
    Failed(String),
}

/// Continuous capture into one immutable PipeWire generation.
///
/// Drop initiates shutdown; keep the Tokio runtime alive for cleanup. Explicit
/// shutdown joins both actors and returns the capture descriptor's owner, so
/// a broker session may then be released. Consumer-held destinations are never
/// reused on uncertain teardown. Frame cadence belongs to the source config.
pub struct Video<F> {
    identity: VideoNodeIdentity,
    state: watch::Receiver<State>,
    commands: mpsc::Sender<Command>,
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<F>>>,
}

impl<F: AsFd + Send + 'static> Video<F> {
    /// Start a source and register its buffers without admitting capture.
    ///
    /// This lets the consumer configure its exact PipeWire target before the
    /// first frame is produced. Call [`Self::activate`] to begin capture.
    pub async fn prepare(
        actor: Actor<F>,
        config: VideoSourceConfig,
        remote: PipeWireRemote,
    ) -> io::Result<Self> {
        Self::prepare_inner(actor, config, remote).await
    }

    /// Start a source and immediately admit capture.
    pub async fn start(
        actor: Actor<F>,
        config: VideoSourceConfig,
        remote: PipeWireRemote,
    ) -> io::Result<Self> {
        let video = Self::prepare_inner(actor, config, remote).await?;
        if let Err(cause) = video.activate().await {
            let _ = video.shutdown().await;
            return Err(cause);
        }
        Ok(video)
    }

    async fn prepare_inner(
        actor: Actor<F>,
        config: VideoSourceConfig,
        remote: PipeWireRemote,
    ) -> io::Result<Self> {
        let period = config
            .frame_rate
            .frame_interval()
            .ok_or_else(|| invalid("capture cadence is not representable"))?;
        let registration = Registration::new(&actor)?;
        let source = VideoSourceActor::spawn().map_err(error)?;
        let identity = source
            .start(VideoSourceGeneration {
                config,
                buffers: registration.export()?,
                remote,
            })
            .await
            .map_err(error)?;
        let output = registration.bind(identity.clone());
        let (state, receive) = watch::channel(State::Active);
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(
            actor,
            Generation {
                source,
                output,
                identity: identity.clone(),
                period,
            },
            command_rx,
            stop.clone(),
            state,
        ));
        Ok(Self {
            identity,
            state: receive,
            commands,
            stop,
            task: Some(task),
        })
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        &self.identity
    }

    pub fn subscribe(&self) -> watch::Receiver<State> {
        self.state.clone()
    }

    /// Admit captures for this immutable source generation.
    pub async fn activate(&self) -> io::Result<()> {
        self.request(CommandKind::Activate).await
    }

    /// Stop admitting captures and discard any result whose request was pending.
    pub async fn suspend(&self) -> io::Result<()> {
        self.request(CommandKind::Suspend).await
    }

    async fn request(&self, kind: CommandKind) -> io::Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command { kind, reply })
            .await
            .map_err(|_| error("capture video owner stopped"))?;
        response
            .await
            .map_err(|_| error("capture video owner stopped"))
    }

    pub async fn shutdown(mut self) -> io::Result<F> {
        self.stop.cancel();
        self.task
            .take()
            .expect("live video owns worker")
            .await
            .map_err(error)?
    }
}

#[derive(Clone, Copy)]
enum CommandKind {
    Activate,
    Suspend,
}

struct Command {
    kind: CommandKind,
    reply: oneshot::Sender<()>,
}

struct Generation {
    source: VideoSourceActor,
    output: Output,
    identity: VideoNodeIdentity,
    period: Duration,
}

impl<F> Drop for Video<F> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn run<F: AsFd + Send + 'static>(
    actor: Actor<F>,
    mut generation: Generation,
    commands: mpsc::Receiver<Command>,
    stop: CancellationToken,
    state: watch::Sender<State>,
) -> io::Result<F> {
    let result = pump(&actor, &mut generation, commands, &stop).await;
    let mut failure = result.err();
    if let Some(failure) = &failure {
        state.send_replace(State::Failed(failure.to_string()));
    }
    // Join the transport even after an event or publication error. The output
    // owner retires every uncertain publication, including lost acknowledgements.
    if let Err(cause) = generation.source.shutdown().await {
        failure.get_or_insert_with(|| error(cause));
    }
    drop(generation.output);
    let owner = actor.shutdown().await;
    let result = match (failure, owner) {
        (Some(failure), _) => Err(failure),
        (None, owner) => owner,
    };
    state.send_replace(match &result {
        Ok(_) => State::Stopped,
        Err(cause) => State::Failed(cause.to_string()),
    });
    result
}

async fn pump<F: AsFd + Send + 'static>(
    actor: &Actor<F>,
    generation: &mut Generation,
    mut commands: mpsc::Receiver<Command>,
    stop: &CancellationToken,
) -> io::Result<()> {
    let mut ready = 0;
    let mut active = false;
    let mut wanted = false;
    let mut first = true;
    let capture = actor.capture();
    tokio::pin!(capture);
    let mut tick = tokio::time::interval(generation.period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Ok(()),
            _ = actor.closed() => return Err(error("capture actor stopped accepting frames")),
            command = commands.recv() => {
                let command = command.ok_or_else(|| error("capture video control closed"))?;
                match command.kind {
                    CommandKind::Activate => active = true,
                    CommandKind::Suspend => {
                        active = false;
                        wanted = false;
                        // A request may already have been admitted. Dropping its
                        // reply future lets the actor reclaim its completed frame
                        // without publishing it after suspension returns.
                        capture.set(actor.capture());
                    }
                }
                let _ = command.reply.send(());
            }
            event = generation.source.next_event() => {
                let event = event.ok_or_else(|| error("capture video source stopped"))?;
                generation.output.handle_event(&event)?;
                match event {
                    VideoSourceActorEvent::BufferAvailable { media_generation, .. } if media_generation == generation.identity.media_generation => ready += 1,
                    VideoSourceActorEvent::GenerationFailed { identity: failed, error: cause, .. } if failed == generation.identity => return Err(error(cause)),
                    _ => (),
                }
            }
            _ = tick.tick(), if active && !wanted && ready == actor.buffers().len() => { wanted = true; }
            result = &mut capture, if active && wanted => {
                wanted = false;
                match result {
                    Ok(frame) => {
                        let pts = i64::try_from(frame.timestamp().as_nanos()).map_err(error)?;
                        let description = generation.output.begin_publish(frame, pts, first)?;
                        // Ownership is recorded before this cancellable handoff.
                        tokio::select! {
                            _ = stop.cancelled() => return Ok(()),
                            _ = actor.closed() => return Err(error("capture actor stopped accepting frames")),
                            result = generation.source.publish(generation.identity.media_generation, description) => result.map_err(error)?,
                        }
                        first = false;
                    }
                    Err(CaptureError::Backpressure) => (),
                    Err(cause) => return Err(error(cause)),
                }
                capture.set(actor.capture());
            }
        }
    }
}

fn error(cause: impl std::fmt::Display) -> io::Error {
    io::Error::other(cause.to_string())
}
