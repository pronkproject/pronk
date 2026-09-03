use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use pronk_media::{EncodedVideoAccessUnit, VideoFrameDependency};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::transport::{
    NegotiatedVideoTransport, VideoSendOutcome, VideoSenderPort, VideoTransportError,
    VideoTransportFeedbackSnapshot, VideoTransportPressure,
};

const COMMAND_CAPACITY: usize = 8;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct VideoSenderStatistics {
    pub frames: u64,
    pub acknowledged_frames: u64,
    pub acknowledged_audio_packets: u64,
    pub key_frames: u64,
    pub encoded_bytes: u64,
    pub dropped_frames: u64,
    pub queue_delay: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoSenderState {
    Empty,
    Configured,
    Streaming,
    Suspended,
    Failed,
    Stopped,
}

#[derive(Debug, Clone)]
struct VideoSenderSnapshot {
    generation: Option<NonZeroU64>,
    state: VideoSenderState,
    statistics: VideoSenderStatistics,
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct VideoSenderFeedbackSnapshot {
    pub revision: u64,
    pub generation: Option<NonZeroU64>,
    pub key_frame_requests: u64,
    pub acknowledged_frames: u64,
    pub pressure: Option<VideoTransportPressure>,
    pub terminal_error: Option<VideoTransportError>,
}

pub(crate) struct VideoSenderActor {
    commands: mpsc::Sender<Command>,
    snapshot: watch::Receiver<VideoSenderSnapshot>,
    feedback: watch::Receiver<VideoSenderFeedbackSnapshot>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for VideoSenderActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoSenderActor")
            .field("snapshot", &self.snapshot.borrow())
            .finish_non_exhaustive()
    }
}

impl VideoSenderActor {
    pub(crate) fn spawn(output: mpsc::Receiver<EncodedVideoAccessUnit>) -> Self {
        let (commands, command_receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (snapshot_tx, snapshot) = watch::channel(VideoSenderSnapshot {
            generation: None,
            state: VideoSenderState::Empty,
            statistics: VideoSenderStatistics::default(),
            last_error: None,
        });
        let (feedback_tx, feedback) = watch::channel(VideoSenderFeedbackSnapshot::default());
        let task = tokio::spawn(run_actor(
            command_receiver,
            output,
            snapshot_tx,
            feedback_tx,
        ));
        Self {
            commands,
            snapshot,
            feedback,
            task: Some(task),
        }
    }

    pub(crate) fn subscribe_feedback(&self) -> watch::Receiver<VideoSenderFeedbackSnapshot> {
        self.feedback.clone()
    }

    pub(crate) async fn configure(
        &self,
        generation: NonZeroU64,
        transport: NegotiatedVideoTransport,
    ) -> Result<(), VideoTransportError> {
        self.request(|reply| Command::Configure {
            generation,
            transport,
            reply,
        })
        .await
    }

    pub(crate) async fn set_target_playout_delay(
        &self,
        generation: NonZeroU64,
        delay: Duration,
    ) -> Result<(), VideoTransportError> {
        self.request(|reply| Command::SetTargetPlayoutDelay {
            generation,
            delay,
            reply,
        })
        .await
    }

    pub(crate) async fn start(&self, generation: NonZeroU64) -> Result<(), VideoTransportError> {
        self.transition(generation, Transition::Start).await
    }

    pub(crate) async fn suspend(&self, generation: NonZeroU64) -> Result<(), VideoTransportError> {
        self.transition(generation, Transition::Suspend).await
    }

    pub(crate) async fn resume(&self, generation: NonZeroU64) -> Result<(), VideoTransportError> {
        self.transition(generation, Transition::Resume).await
    }

    async fn transition(
        &self,
        generation: NonZeroU64,
        transition: Transition,
    ) -> Result<(), VideoTransportError> {
        self.request(|reply| Command::Transition {
            generation,
            transition,
            reply,
        })
        .await
    }

    pub(crate) async fn stop(
        &self,
        generation: NonZeroU64,
    ) -> Result<VideoSenderStatistics, VideoTransportError> {
        self.request(|reply| Command::Stop { generation, reply })
            .await
    }

    pub(crate) async fn statistics(
        &self,
        generation: NonZeroU64,
    ) -> Result<VideoSenderStatistics, VideoTransportError> {
        self.request(|reply| Command::Statistics { generation, reply })
            .await
    }

    pub(crate) async fn wait_for_frame_after(
        &self,
        generation: NonZeroU64,
        previous: u64,
        timeout: Duration,
    ) -> Result<(), VideoTransportError> {
        let mut snapshot = self.snapshot.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let current = snapshot.borrow().clone();
                if current.generation != Some(generation) {
                    return Err(VideoTransportError::new(format!(
                        "video sender generation changed while waiting for {generation}"
                    )));
                }
                if current.state == VideoSenderState::Failed {
                    return Err(VideoTransportError::new(current.last_error.unwrap_or_else(
                        || "video sender failed without diagnostic detail".into(),
                    )));
                }
                if current.statistics.frames > previous {
                    return Ok(());
                }
                snapshot
                    .changed()
                    .await
                    .map_err(|_| VideoTransportError::new("video sender actor stopped"))?;
            }
        })
        .await
        .map_err(|_| VideoTransportError::new("timed out waiting for encoded video delivery"))?
    }

    pub(crate) async fn wait_for_receiver_ack_after(
        &self,
        generation: NonZeroU64,
        previous: u64,
        timeout: Duration,
    ) -> Result<(), VideoTransportError> {
        let mut snapshot = self.snapshot.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let current = snapshot.borrow().clone();
                if current.generation != Some(generation) {
                    return Err(VideoTransportError::new(format!(
                        "video sender generation changed while waiting for {generation}"
                    )));
                }
                if current.state == VideoSenderState::Failed {
                    return Err(VideoTransportError::new(current.last_error.unwrap_or_else(
                        || "video sender failed without diagnostic detail".into(),
                    )));
                }
                if current.statistics.acknowledged_frames > previous {
                    return Ok(());
                }
                snapshot
                    .changed()
                    .await
                    .map_err(|_| VideoTransportError::new("video sender actor stopped"))?;
            }
        })
        .await
        .map_err(|_| {
            VideoTransportError::new("timed out waiting for Cast receiver video acknowledgement")
        })?
    }

    pub(crate) async fn wait_for_receiver_audio_ack_after(
        &self,
        generation: NonZeroU64,
        previous: u64,
        timeout: Duration,
    ) -> Result<(), VideoTransportError> {
        let mut snapshot = self.snapshot.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let current = snapshot.borrow().clone();
                if current.generation != Some(generation) {
                    return Err(VideoTransportError::new(format!(
                        "video sender generation changed while waiting for audio acknowledgement for {generation}"
                    )));
                }
                if current.state == VideoSenderState::Failed {
                    return Err(VideoTransportError::new(current.last_error.unwrap_or_else(
                        || "video sender failed without diagnostic detail".into(),
                    )));
                }
                if current.statistics.acknowledged_audio_packets > previous {
                    return Ok(());
                }
                snapshot
                    .changed()
                    .await
                    .map_err(|_| VideoTransportError::new("video sender actor stopped"))?;
            }
        })
        .await
        .map_err(|_| {
            VideoTransportError::new("timed out waiting for Cast receiver audio acknowledgement")
        })?
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), VideoTransportError> {
        let result = self
            .request(|reply| Command::Shutdown { reply: Some(reply) })
            .await;
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                VideoTransportError::new(format!("join video sender actor: {error}"))
            })?;
        }
        result
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, VideoTransportError>>) -> Command,
    ) -> Result<T, VideoTransportError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| VideoTransportError::new("video sender command channel closed"))?;
        response
            .await
            .map_err(|_| VideoTransportError::new("video sender reply channel closed"))?
    }
}

impl Drop for VideoSenderActor {
    fn drop(&mut self) {
        let _ = self.commands.try_send(Command::Shutdown { reply: None });
        if let Some(task) = self.task.take() {
            // Ordered shutdown is explicit. Never detach the task if its
            // bounded command queue is full or its owner disappears.
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Transition {
    Start,
    Suspend,
    Resume,
}

enum Command {
    Configure {
        generation: NonZeroU64,
        transport: NegotiatedVideoTransport,
        reply: oneshot::Sender<Result<(), VideoTransportError>>,
    },
    Transition {
        generation: NonZeroU64,
        transition: Transition,
        reply: oneshot::Sender<Result<(), VideoTransportError>>,
    },
    SetTargetPlayoutDelay {
        generation: NonZeroU64,
        delay: Duration,
        reply: oneshot::Sender<Result<(), VideoTransportError>>,
    },
    Stop {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<VideoSenderStatistics, VideoTransportError>>,
    },
    Statistics {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<VideoSenderStatistics, VideoTransportError>>,
    },
    Shutdown {
        reply: Option<oneshot::Sender<Result<(), VideoTransportError>>>,
    },
}

struct ActiveSender {
    generation: NonZeroU64,
    state: VideoSenderState,
    sender: Option<Box<dyn VideoSenderPort>>,
    feedback: watch::Receiver<VideoTransportFeedbackSnapshot>,
    feedback_open: bool,
    observed_key_frame_requests: u64,
    pressure_limited: bool,
    needs_key_frame: bool,
    statistics: VideoSenderStatistics,
}

enum Next {
    Command(Option<Command>),
    AccessUnit(Option<EncodedVideoAccessUnit>),
    Feedback(Result<(), watch::error::RecvError>),
}

async fn run_actor(
    mut commands: mpsc::Receiver<Command>,
    mut output: mpsc::Receiver<EncodedVideoAccessUnit>,
    snapshot: watch::Sender<VideoSenderSnapshot>,
    feedback: watch::Sender<VideoSenderFeedbackSnapshot>,
) {
    let mut active: Option<ActiveSender> = None;
    let mut completed_generation = None;

    loop {
        let next = match active.as_mut() {
            Some(current)
                if current.feedback_open && current.state == VideoSenderState::Streaming =>
            {
                tokio::select! {
                    biased;
                    command = commands.recv() => Next::Command(command),
                    changed = current.feedback.changed() => Next::Feedback(changed),
                    access_unit = output.recv() => Next::AccessUnit(access_unit),
                }
            }
            Some(current) if current.feedback_open => {
                tokio::select! {
                    biased;
                    command = commands.recv() => Next::Command(command),
                    changed = current.feedback.changed() => Next::Feedback(changed),
                }
            }
            Some(current) if current.state == VideoSenderState::Streaming => {
                tokio::select! {
                    biased;
                    command = commands.recv() => Next::Command(command),
                    access_unit = output.recv() => Next::AccessUnit(access_unit),
                }
            }
            Some(_) | None => Next::Command(commands.recv().await),
        };

        let command = match next {
            Next::Command(command) => command,
            Next::AccessUnit(access_unit) => {
                let result = match access_unit {
                    Some(access_unit) => {
                        forward_access_unit(&mut active, access_unit, &snapshot, &feedback).await
                    }
                    None => Err(VideoTransportError::new(
                        "encoded-video source channel closed",
                    )),
                };
                if let Err(error) = result {
                    fail_active(&mut active, &snapshot, &feedback, error).await;
                }
                continue;
            }
            Next::Feedback(Ok(())) => {
                if let Err(error) = process_transport_feedback(&mut active, &snapshot, &feedback) {
                    fail_active(&mut active, &snapshot, &feedback, error).await;
                }
                continue;
            }
            Next::Feedback(Err(_)) => {
                fail_active(
                    &mut active,
                    &snapshot,
                    &feedback,
                    VideoTransportError::new("Cast sender feedback channel closed"),
                )
                .await;
                continue;
            }
        };

        let Some(command) = command else {
            let _ = shutdown_active(&mut active).await;
            publish(
                &snapshot,
                completed_generation,
                VideoSenderState::Stopped,
                VideoSenderStatistics::default(),
                None,
            );
            return;
        };

        match command {
            Command::Configure {
                generation,
                transport,
                reply,
            } => {
                let result = configure_active(
                    &mut active,
                    &mut output,
                    completed_generation,
                    generation,
                    transport,
                    &snapshot,
                    &feedback,
                )
                .await;
                let _ = reply.send(result);
            }
            Command::Transition {
                generation,
                transition: requested,
                reply,
            } => {
                let result = transition_active(&mut active, generation, requested, &snapshot);
                let _ = reply.send(result);
            }
            Command::SetTargetPlayoutDelay {
                generation,
                delay,
                reply,
            } => {
                let result = set_target_playout_delay(&mut active, generation, delay).await;
                let _ = reply.send(result);
            }
            Command::Stop { generation, reply } => {
                let result = stop_active(&mut active, generation, completed_generation).await;
                if result.is_ok() {
                    completed_generation = Some(generation);
                }
                let statistics = result.as_ref().cloned().unwrap_or_default();
                publish(
                    &snapshot,
                    Some(generation),
                    VideoSenderState::Empty,
                    statistics,
                    result.as_ref().err().map(ToString::to_string),
                );
                let _ = reply.send(result);
            }
            Command::Statistics { generation, reply } => {
                let result =
                    active_statistics(&active, generation, completed_generation, &snapshot);
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                let result = shutdown_active(&mut active).await;
                publish(
                    &snapshot,
                    completed_generation,
                    VideoSenderState::Stopped,
                    VideoSenderStatistics::default(),
                    result.as_ref().err().map(ToString::to_string),
                );
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                return;
            }
        }
    }
}

async fn configure_active(
    active: &mut Option<ActiveSender>,
    output: &mut mpsc::Receiver<EncodedVideoAccessUnit>,
    completed_generation: Option<NonZeroU64>,
    generation: NonZeroU64,
    transport: NegotiatedVideoTransport,
    snapshot: &watch::Sender<VideoSenderSnapshot>,
    feedback_snapshot: &watch::Sender<VideoSenderFeedbackSnapshot>,
) -> Result<(), VideoTransportError> {
    let NegotiatedVideoTransport {
        video_codec: _,
        sender,
        audio_sender,
        feedback,
        minimum_bitrate: _,
    } = transport;
    if let Some(audio_sender) = audio_sender {
        let _ = audio_sender.shutdown().await;
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(
            "audio sender was not split from negotiated transport before configuring video",
        ));
    }
    if active.is_some() {
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(
            "a video sender generation is already active",
        ));
    }
    if completed_generation.is_some_and(|done| generation <= done) {
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(format!(
            "video sender generation {generation} is not newer than {completed_generation:?}"
        )));
    }
    while output.try_recv().is_ok() {}
    *active = Some(ActiveSender {
        generation,
        state: VideoSenderState::Configured,
        sender: Some(sender),
        feedback,
        feedback_open: true,
        observed_key_frame_requests: 0,
        pressure_limited: false,
        needs_key_frame: false,
        statistics: VideoSenderStatistics::default(),
    });
    let feedback_revision = feedback_snapshot.borrow().revision.saturating_add(1);
    feedback_snapshot.send_replace(VideoSenderFeedbackSnapshot {
        revision: feedback_revision,
        generation: Some(generation),
        key_frame_requests: 0,
        acknowledged_frames: 0,
        pressure: None,
        terminal_error: None,
    });
    publish(
        snapshot,
        Some(generation),
        VideoSenderState::Configured,
        VideoSenderStatistics::default(),
        None,
    );
    Ok(())
}

fn process_transport_feedback(
    active: &mut Option<ActiveSender>,
    snapshot: &watch::Sender<VideoSenderSnapshot>,
    feedback: &watch::Sender<VideoSenderFeedbackSnapshot>,
) -> Result<(), VideoTransportError> {
    let Some(active) = active.as_mut() else {
        return Ok(());
    };
    let incoming = active.feedback.borrow_and_update().clone();
    if let Some(error) = incoming.terminal_error {
        return Err(error);
    }
    active.statistics.acknowledged_frames = incoming.acknowledged_frames;
    active.statistics.acknowledged_audio_packets = incoming.acknowledged_audio_packets;
    let mut request_key_frame = false;
    if incoming.key_frame_requests > active.observed_key_frame_requests {
        active.observed_key_frame_requests = incoming.key_frame_requests;
        active.needs_key_frame = true;
        request_key_frame = true;
    }
    if let Some(pressure) = incoming.pressure {
        let limited = pressure.queue_saturated();
        if limited != active.pressure_limited {
            active.pressure_limited = limited;
            active.needs_key_frame = true;
            // An established stream rejects every frame while pressure is
            // active, including the key frame this request would produce.
            // Ask for recovery only after the transport can admit it again.
            request_key_frame |= !limited;
        }
        if incoming.acknowledged_frames == 0 && pressure.in_flight_frames != 0 {
            active.needs_key_frame = true;
            request_key_frame = true;
        }
    }
    publish_feedback(
        feedback,
        active.generation,
        request_key_frame,
        incoming.acknowledged_frames,
        incoming.pressure,
    );
    publish(
        snapshot,
        Some(active.generation),
        active.state,
        active.statistics.clone(),
        None,
    );
    Ok(())
}

async fn forward_access_unit(
    active: &mut Option<ActiveSender>,
    access_unit: EncodedVideoAccessUnit,
    snapshot: &watch::Sender<VideoSenderSnapshot>,
    feedback: &watch::Sender<VideoSenderFeedbackSnapshot>,
) -> Result<(), VideoTransportError> {
    let active = active
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("video sender is missing"))?;
    if access_unit.media_generation != active.generation {
        active.statistics.dropped_frames = active.statistics.dropped_frames.saturating_add(1);
        return Ok(());
    }
    if (active.pressure_limited
        && (active.statistics.acknowledged_frames != 0
            || access_unit.dependency != VideoFrameDependency::KeyFrame))
        || (active.needs_key_frame && access_unit.dependency == VideoFrameDependency::Delta)
    {
        active.statistics.dropped_frames = active.statistics.dropped_frames.saturating_add(1);
        publish(
            snapshot,
            Some(active.generation),
            active.state,
            active.statistics.clone(),
            None,
        );
        return Ok(());
    }
    let bytes = access_unit.data.len() as u64;
    let key_frame = access_unit.dependency == VideoFrameDependency::KeyFrame;
    let queue_delay = Instant::now()
        .checked_duration_since(access_unit.reference_time)
        .unwrap_or_default();
    let sender = active
        .sender
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("video sender transport is missing"))?;
    let outcome = tokio::time::timeout(SEND_TIMEOUT, sender.send(access_unit))
        .await
        .map_err(|_| VideoTransportError::new("timed out enqueueing encoded video"))??;
    if outcome == VideoSendOutcome::Congested {
        active.statistics.dropped_frames = active.statistics.dropped_frames.saturating_add(1);
        active.statistics.queue_delay = queue_delay;
        active.pressure_limited = true;
        active.needs_key_frame = true;
        publish_feedback(
            feedback,
            active.generation,
            true,
            active.statistics.acknowledged_frames,
            None,
        );
        publish(
            snapshot,
            Some(active.generation),
            active.state,
            active.statistics.clone(),
            None,
        );
        return Ok(());
    }
    active.statistics.frames = active.statistics.frames.saturating_add(1);
    active.statistics.key_frames = active
        .statistics
        .key_frames
        .saturating_add(u64::from(key_frame));
    active.statistics.encoded_bytes = active.statistics.encoded_bytes.saturating_add(bytes);
    active.statistics.queue_delay = queue_delay;
    if key_frame {
        active.needs_key_frame = false;
    }
    publish(
        snapshot,
        Some(active.generation),
        active.state,
        active.statistics.clone(),
        None,
    );
    Ok(())
}

fn publish_feedback(
    feedback: &watch::Sender<VideoSenderFeedbackSnapshot>,
    generation: NonZeroU64,
    request_key_frame: bool,
    acknowledged_frames: u64,
    pressure: Option<VideoTransportPressure>,
) {
    feedback.send_modify(|snapshot| {
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.generation = Some(generation);
        snapshot.key_frame_requests = snapshot
            .key_frame_requests
            .saturating_add(u64::from(request_key_frame));
        snapshot.acknowledged_frames = acknowledged_frames;
        if pressure.is_some() {
            snapshot.pressure = pressure;
        }
    });
}

async fn fail_active(
    active: &mut Option<ActiveSender>,
    snapshot: &watch::Sender<VideoSenderSnapshot>,
    feedback: &watch::Sender<VideoSenderFeedbackSnapshot>,
    error: VideoTransportError,
) {
    let Some(active) = active.as_mut() else {
        return;
    };
    active.state = VideoSenderState::Failed;
    active.feedback_open = false;
    if let Some(sender) = active.sender.take() {
        let _ = sender.shutdown().await;
    }
    feedback.send_modify(|current| {
        current.revision = current.revision.saturating_add(1);
        current.generation = Some(active.generation);
        current.terminal_error = Some(error.clone());
    });
    publish(
        snapshot,
        Some(active.generation),
        active.state,
        active.statistics.clone(),
        Some(error.to_string()),
    );
}

fn transition_active(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
    transition: Transition,
    snapshot: &watch::Sender<VideoSenderSnapshot>,
) -> Result<(), VideoTransportError> {
    let active = matching_active(active, generation)?;
    let (required, desired) = match transition {
        Transition::Start => (VideoSenderState::Configured, VideoSenderState::Streaming),
        Transition::Suspend => (VideoSenderState::Streaming, VideoSenderState::Suspended),
        Transition::Resume => (VideoSenderState::Suspended, VideoSenderState::Streaming),
    };
    if active.state != required {
        return Err(VideoTransportError::new(format!(
            "video sender generation {generation} is {:?}; expected {required:?}",
            active.state
        )));
    }
    active.state = desired;
    publish(
        snapshot,
        Some(generation),
        desired,
        active.statistics.clone(),
        None,
    );
    Ok(())
}

async fn set_target_playout_delay(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
    delay: Duration,
) -> Result<(), VideoTransportError> {
    matching_active(active, generation)?
        .sender
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("video sender transport is missing"))?
        .set_target_playout_delay(delay)
        .await
}

async fn stop_active(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
    completed_generation: Option<NonZeroU64>,
) -> Result<VideoSenderStatistics, VideoTransportError> {
    let Some(current) = active.as_ref() else {
        if completed_generation == Some(generation) {
            return Ok(VideoSenderStatistics::default());
        }
        return Err(VideoTransportError::new(
            "there is no matching video sender generation to stop",
        ));
    };
    if current.generation != generation {
        return Err(generation_mismatch(current.generation, generation));
    }
    let mut current = active.take().expect("active sender checked above");
    let statistics = current.statistics.clone();
    if let Some(sender) = current.sender.take() {
        sender.shutdown().await?;
    }
    Ok(statistics)
}

async fn shutdown_active(active: &mut Option<ActiveSender>) -> Result<(), VideoTransportError> {
    let Some(mut active) = active.take() else {
        return Ok(());
    };
    match active.sender.take() {
        Some(sender) => sender.shutdown().await,
        None => Ok(()),
    }
}

fn active_statistics(
    active: &Option<ActiveSender>,
    generation: NonZeroU64,
    completed_generation: Option<NonZeroU64>,
    snapshot: &watch::Sender<VideoSenderSnapshot>,
) -> Result<VideoSenderStatistics, VideoTransportError> {
    match active {
        Some(active) if active.generation != generation => {
            Err(generation_mismatch(active.generation, generation))
        }
        Some(active) if active.state == VideoSenderState::Failed => Err(VideoTransportError::new(
            snapshot
                .borrow()
                .last_error
                .clone()
                .unwrap_or_else(|| "video sender failed without diagnostic detail".into()),
        )),
        Some(active) => Ok(active.statistics.clone()),
        None if completed_generation == Some(generation) => {
            Ok(snapshot.borrow().statistics.clone())
        }
        None => Err(VideoTransportError::new(
            "there is no matching video sender generation for statistics",
        )),
    }
}

fn matching_active(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
) -> Result<&mut ActiveSender, VideoTransportError> {
    let active = active
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("there is no active video sender generation"))?;
    if active.generation != generation {
        return Err(generation_mismatch(active.generation, generation));
    }
    Ok(active)
}

fn generation_mismatch(active: NonZeroU64, requested: NonZeroU64) -> VideoTransportError {
    VideoTransportError::new(format!(
        "requested video sender generation {requested}; active generation is {active}"
    ))
}

fn publish(
    snapshot: &watch::Sender<VideoSenderSnapshot>,
    generation: Option<NonZeroU64>,
    state: VideoSenderState,
    statistics: VideoSenderStatistics,
    last_error: Option<String>,
) {
    snapshot.send_modify(|current| {
        current.generation = generation;
        current.state = state;
        current.statistics = statistics;
        current.last_error = last_error;
    });
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use pronk_media::{EncodedVideoAccessUnit, VideoFrameDependency};

    use super::VideoSenderActor;
    use crate::transport::{
        NegotiatedVideoTransport, VideoSendOutcome, VideoSenderPort, VideoTransportError,
        VideoTransportFeedbackSnapshot, VideoTransportPressure,
    };

    #[derive(Debug)]
    struct AcceptingSender {
        _feedback: tokio::sync::watch::Sender<VideoTransportFeedbackSnapshot>,
    }

    #[derive(Debug)]
    struct RecordingSender {
        playout_delays: Arc<Mutex<Vec<Duration>>>,
    }

    #[async_trait::async_trait]
    impl VideoSenderPort for AcceptingSender {
        async fn send(
            &mut self,
            _access_unit: EncodedVideoAccessUnit,
        ) -> Result<VideoSendOutcome, VideoTransportError> {
            Ok(VideoSendOutcome::Accepted)
        }

        async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl VideoSenderPort for RecordingSender {
        fn supports_target_playout_delay_updates(&self) -> bool {
            true
        }

        async fn set_target_playout_delay(
            &mut self,
            delay: Duration,
        ) -> Result<(), VideoTransportError> {
            self.playout_delays.lock().unwrap().push(delay);
            Ok(())
        }

        async fn send(
            &mut self,
            _access_unit: EncodedVideoAccessUnit,
        ) -> Result<VideoSendOutcome, VideoTransportError> {
            Ok(VideoSendOutcome::Accepted)
        }

        async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn playout_delay_updates_are_generation_scoped() {
        let (_output, receiver) = tokio::sync::mpsc::channel(1);
        let actor = VideoSenderActor::spawn(receiver);
        let generation = NonZeroU64::new(2).unwrap();
        let playout_delays = Arc::new(Mutex::new(Vec::new()));
        let (_feedback, feedback) =
            tokio::sync::watch::channel(VideoTransportFeedbackSnapshot::default());
        actor
            .configure(
                generation,
                NegotiatedVideoTransport {
                    video_codec: pronk_media::VideoCodec::Vp8,
                    sender: Box::new(RecordingSender {
                        playout_delays: playout_delays.clone(),
                    }),
                    audio_sender: None,
                    feedback,
                    minimum_bitrate: None,
                },
            )
            .await
            .unwrap();

        assert!(actor
            .set_target_playout_delay(NonZeroU64::new(1).unwrap(), Duration::from_millis(66),)
            .await
            .unwrap_err()
            .to_string()
            .contains("active generation is 2"));
        actor
            .set_target_playout_delay(generation, Duration::from_millis(66))
            .await
            .unwrap();
        assert_eq!(*playout_delays.lock().unwrap(), [Duration::from_millis(66)]);

        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn routes_only_the_active_generation_and_waits_for_delivery() {
        let (output, receiver) = tokio::sync::mpsc::channel(4);
        let actor = VideoSenderActor::spawn(receiver);
        let generation = NonZeroU64::new(2).unwrap();
        actor
            .configure(generation, accepting_transport())
            .await
            .unwrap();
        actor.start(generation).await.unwrap();

        output.send(access_unit(1)).await.unwrap();
        output.send(access_unit(2)).await.unwrap();
        actor
            .wait_for_frame_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();
        let statistics = actor.statistics(generation).await.unwrap();
        assert_eq!(statistics.frames, 1);
        assert_eq!(statistics.dropped_frames, 1);
        assert_eq!(statistics.key_frames, 1);
        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn receiver_acknowledgement_is_distinct_from_local_delivery() {
        let (output, receiver) = tokio::sync::mpsc::channel(4);
        let actor = VideoSenderActor::spawn(receiver);
        let (transport, transport_feedback) = accepting_transport_with_feedback();
        let generation = NonZeroU64::new(3).unwrap();
        actor.configure(generation, transport).await.unwrap();
        actor.start(generation).await.unwrap();

        output.send(access_unit(3)).await.unwrap();
        actor
            .wait_for_frame_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(actor
            .wait_for_receiver_ack_after(generation, 0, Duration::from_millis(10))
            .await
            .unwrap_err()
            .to_string()
            .contains("receiver video acknowledgement"));

        transport_feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_frames = 1;
        });
        actor
            .wait_for_receiver_ack_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            actor
                .statistics(generation)
                .await
                .unwrap()
                .acknowledged_frames,
            1
        );
        assert!(actor
            .wait_for_receiver_audio_ack_after(generation, 0, Duration::from_millis(10))
            .await
            .unwrap_err()
            .to_string()
            .contains("receiver audio acknowledgement"));
        transport_feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_audio_packets = 1;
        });
        actor
            .wait_for_receiver_audio_ack_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            actor
                .statistics(generation)
                .await
                .unwrap()
                .acknowledged_audio_packets,
            1
        );
        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn terminal_transport_feedback_fails_without_another_access_unit() {
        let (_output, receiver) = tokio::sync::mpsc::channel(4);
        let actor = VideoSenderActor::spawn(receiver);
        let mut projected = actor.subscribe_feedback();
        let (transport, transport_feedback) = accepting_transport_with_feedback();
        let generation = NonZeroU64::new(4).unwrap();
        actor.configure(generation, transport).await.unwrap();
        actor.start(generation).await.unwrap();

        transport_feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.terminal_error = Some(VideoTransportError::new(
                "scripted receiver acknowledgement timeout",
            ));
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if projected.borrow().terminal_error.is_some() {
                    break;
                }
                projected.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(actor
            .statistics(generation)
            .await
            .unwrap_err()
            .to_string()
            .contains("scripted receiver acknowledgement timeout"));
        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_pressure_retries_and_admits_key_frames_until_first_ack() {
        let (output, receiver) = tokio::sync::mpsc::channel(8);
        let actor = VideoSenderActor::spawn(receiver);
        let mut projected = actor.subscribe_feedback();
        let (transport, transport_feedback) = accepting_transport_with_feedback();
        let generation = NonZeroU64::new(5).unwrap();
        actor.configure(generation, transport).await.unwrap();
        actor.start(generation).await.unwrap();
        output.send(access_unit(5)).await.unwrap();
        actor
            .wait_for_frame_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();

        for revision in 1..=2 {
            transport_feedback.send_modify(|snapshot| {
                snapshot.revision = revision;
                snapshot.pressure = Some(VideoTransportPressure {
                    in_flight_frames: 2,
                    in_flight_media_duration: Duration::from_millis(500),
                    max_acceptable_in_flight_duration: Duration::from_millis(100),
                    ..VideoTransportPressure::default()
                });
            });
            wait_for_key_frame_requests(&mut projected, revision).await;
            output
                .send(access_unit_with_dependency(
                    5,
                    VideoFrameDependency::KeyFrame,
                ))
                .await
                .unwrap();
            actor
                .wait_for_frame_after(generation, revision, Duration::from_secs(1))
                .await
                .unwrap();
        }

        let statistics = actor.statistics(generation).await.unwrap();
        assert_eq!(statistics.frames, 3);
        assert_eq!(statistics.key_frames, 3);
        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn established_pressure_requests_a_key_frame_only_after_recovery() {
        let (output, receiver) = tokio::sync::mpsc::channel(8);
        let actor = VideoSenderActor::spawn(receiver);
        let mut feedback = actor.subscribe_feedback();
        let (transport, transport_feedback) = accepting_transport_with_feedback();
        let generation = NonZeroU64::new(2).unwrap();
        actor.configure(generation, transport).await.unwrap();
        actor.start(generation).await.unwrap();
        output
            .send(access_unit_with_dependency(
                2,
                VideoFrameDependency::KeyFrame,
            ))
            .await
            .unwrap();
        actor
            .wait_for_frame_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();

        transport_feedback.send_modify(|snapshot| {
            snapshot.revision = 1;
            snapshot.acknowledged_frames = 1;
            snapshot.pressure = Some(VideoTransportPressure {
                in_flight_frames: 2,
                in_flight_media_duration: Duration::from_millis(200),
                max_acceptable_in_flight_duration: Duration::from_millis(100),
                ..VideoTransportPressure::default()
            });
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if feedback
                    .borrow()
                    .pressure
                    .is_some_and(|pressure| pressure.queue_saturated())
                {
                    break;
                }
                feedback.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(feedback.borrow().key_frame_requests, 0);
        output
            .send(access_unit_with_dependency(2, VideoFrameDependency::Delta))
            .await
            .unwrap();

        transport_feedback.send_modify(|snapshot| {
            snapshot.revision = 2;
            snapshot.acknowledged_frames = 1;
            snapshot.pressure = Some(VideoTransportPressure {
                max_acceptable_in_flight_duration: Duration::from_millis(100),
                ..VideoTransportPressure::default()
            });
        });
        wait_for_key_frame_requests(&mut feedback, 1).await;
        output
            .send(access_unit_with_dependency(2, VideoFrameDependency::Delta))
            .await
            .unwrap();
        output
            .send(access_unit_with_dependency(
                2,
                VideoFrameDependency::KeyFrame,
            ))
            .await
            .unwrap();
        actor
            .wait_for_frame_after(generation, 1, Duration::from_secs(1))
            .await
            .unwrap();
        let statistics = actor.statistics(generation).await.unwrap();
        assert_eq!(statistics.frames, 2);
        assert_eq!(statistics.dropped_frames, 2);
        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    fn accepting_transport() -> NegotiatedVideoTransport {
        let (feedback, receiver) =
            tokio::sync::watch::channel(VideoTransportFeedbackSnapshot::default());
        NegotiatedVideoTransport {
            video_codec: pronk_media::VideoCodec::Vp8,
            sender: Box::new(AcceptingSender {
                _feedback: feedback,
            }),
            audio_sender: None,
            feedback: receiver,
            minimum_bitrate: None,
        }
    }

    fn accepting_transport_with_feedback() -> (
        NegotiatedVideoTransport,
        tokio::sync::watch::Sender<VideoTransportFeedbackSnapshot>,
    ) {
        let (feedback, receiver) =
            tokio::sync::watch::channel(VideoTransportFeedbackSnapshot::default());
        (
            NegotiatedVideoTransport {
                video_codec: pronk_media::VideoCodec::Vp8,
                sender: Box::new(AcceptingSender {
                    _feedback: feedback.clone(),
                }),
                audio_sender: None,
                feedback: receiver,
                minimum_bitrate: None,
            },
            feedback,
        )
    }

    async fn wait_for_key_frame_requests(
        feedback: &mut tokio::sync::watch::Receiver<super::VideoSenderFeedbackSnapshot>,
        expected: u64,
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if feedback.borrow().key_frame_requests >= expected {
                    return;
                }
                feedback.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    fn access_unit(generation: u64) -> EncodedVideoAccessUnit {
        access_unit_with_dependency(generation, VideoFrameDependency::KeyFrame)
    }

    fn access_unit_with_dependency(
        generation: u64,
        dependency: VideoFrameDependency,
    ) -> EncodedVideoAccessUnit {
        EncodedVideoAccessUnit {
            media_generation: NonZeroU64::new(generation).unwrap(),
            dependency,
            data: vec![0, 0, 0, 1, 0x65],
            media_timestamp: Duration::ZERO,
            reference_time: Instant::now(),
            duration: Duration::from_millis(16),
        }
    }
}
