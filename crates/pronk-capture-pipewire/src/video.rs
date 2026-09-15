use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use pronk_capture::{Actor, CaptureError};
use pronk_pipewire::{
    PipeWireRemote, VideoNodeIdentity, VideoSourceActor, VideoSourceActorEvent, VideoSourceConfig,
    VideoSourceGeneration,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{invalid, Output, Registration};

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
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<F>>>,
}

impl<F: AsFd + Send + 'static> Video<F> {
    pub async fn start(
        actor: Actor<F>,
        config: VideoSourceConfig,
        remote: PipeWireRemote,
    ) -> io::Result<Self> {
        let period = Duration::from_secs(1) / config.refresh_hz.get();
        if period.is_zero() {
            return Err(invalid("capture cadence is not representable"));
        }
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
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(
            actor,
            source,
            output,
            identity.clone(),
            period,
            stop.clone(),
            state,
        ));
        Ok(Self {
            identity,
            state: receive,
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

    pub async fn shutdown(mut self) -> io::Result<F> {
        self.stop.cancel();
        self.task
            .take()
            .expect("live video owns worker")
            .await
            .map_err(error)?
    }
}

impl<F> Drop for Video<F> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn run<F: AsFd + Send + 'static>(
    actor: Actor<F>,
    mut source: VideoSourceActor,
    mut output: Output,
    identity: VideoNodeIdentity,
    period: Duration,
    stop: CancellationToken,
    state: watch::Sender<State>,
) -> io::Result<F> {
    let result = pump(&actor, &mut source, &mut output, &identity, period, &stop).await;
    let mut failure = result.err();
    if let Some(failure) = &failure {
        state.send_replace(State::Failed(failure.to_string()));
    }
    // Join the transport even after an event or publication error. The output
    // owner retires every uncertain publication, including lost acknowledgements.
    if let Err(cause) = source.shutdown().await {
        failure.get_or_insert_with(|| error(cause));
    }
    drop(output);
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
    source: &mut VideoSourceActor,
    output: &mut Output,
    identity: &VideoNodeIdentity,
    period: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    let mut ready = 0;
    let mut wanted = false;
    let mut first = true;
    let capture = actor.capture();
    tokio::pin!(capture);
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = actor.closed() => return Err(error("capture actor stopped accepting frames")),
            event = source.next_event() => {
                let event = event.ok_or_else(|| error("capture video source stopped"))?;
                output.handle_event(&event)?;
                match event {
                    VideoSourceActorEvent::BufferAvailable { media_generation, .. } if media_generation == identity.media_generation => ready += 1,
                    VideoSourceActorEvent::GenerationFailed { identity: failed, error: cause, .. } if failed == *identity => return Err(error(cause)),
                    _ => (),
                }
            }
            _ = tick.tick(), if !wanted && ready == actor.buffers().len() => { wanted = true; }
            result = &mut capture, if wanted => {
                wanted = false;
                match result {
                    Ok(frame) => {
                        let pts = i64::try_from(frame.timestamp().as_nanos()).map_err(error)?;
                        let description = output.begin_publish(frame, pts, first)?;
                        // Ownership is recorded before this cancellable handoff.
                        tokio::select! {
                            _ = stop.cancelled() => return Ok(()),
                            _ = actor.closed() => return Err(error("capture actor stopped accepting frames")),
                            result = source.publish(identity.media_generation, description) => result.map_err(error)?,
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
