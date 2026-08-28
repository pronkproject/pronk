use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use pronk_media::EncodedAudioPacket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::transport::{AudioSendOutcome, AudioSenderPort, VideoTransportError};

const COMMAND_CAPACITY: usize = 8;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AudioSenderStatistics {
    pub packets: u64,
    pub encoded_bytes: u64,
    pub dropped_packets: u64,
    pub queue_delay: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioSenderState {
    Empty,
    Configured,
    Streaming,
    Suspended,
    Failed,
    Stopped,
}

#[derive(Debug, Clone)]
struct AudioSenderSnapshot {
    generation: Option<NonZeroU64>,
    state: AudioSenderState,
    statistics: AudioSenderStatistics,
    last_error: Option<String>,
}

pub(crate) struct AudioSenderActor {
    commands: mpsc::Sender<Command>,
    snapshot: watch::Receiver<AudioSenderSnapshot>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AudioSenderActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioSenderActor")
            .field("snapshot", &self.snapshot.borrow())
            .finish_non_exhaustive()
    }
}

impl AudioSenderActor {
    pub(crate) fn spawn(output: mpsc::Receiver<EncodedAudioPacket>) -> Self {
        let (commands, command_receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (snapshot_tx, snapshot) = watch::channel(AudioSenderSnapshot {
            generation: None,
            state: AudioSenderState::Empty,
            statistics: AudioSenderStatistics::default(),
            last_error: None,
        });
        let task = tokio::spawn(run_actor(command_receiver, output, snapshot_tx));
        Self {
            commands,
            snapshot,
            task: Some(task),
        }
    }

    pub(crate) async fn configure(
        &self,
        generation: NonZeroU64,
        sender: Box<dyn AudioSenderPort>,
    ) -> Result<(), VideoTransportError> {
        self.request(|reply| Command::Configure {
            generation,
            sender,
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
    ) -> Result<AudioSenderStatistics, VideoTransportError> {
        self.request(|reply| Command::Stop { generation, reply })
            .await
    }

    pub(crate) async fn statistics(
        &self,
        generation: NonZeroU64,
    ) -> Result<AudioSenderStatistics, VideoTransportError> {
        self.request(|reply| Command::Statistics { generation, reply })
            .await
    }

    pub(crate) async fn wait_for_packet_after(
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
                        "audio sender generation changed while waiting for {generation}"
                    )));
                }
                if current.state == AudioSenderState::Failed {
                    return Err(VideoTransportError::new(current.last_error.unwrap_or_else(
                        || "audio sender failed without diagnostic detail".into(),
                    )));
                }
                if current.statistics.packets > previous {
                    return Ok(());
                }
                snapshot
                    .changed()
                    .await
                    .map_err(|_| VideoTransportError::new("audio sender actor stopped"))?;
            }
        })
        .await
        .map_err(|_| VideoTransportError::new("timed out waiting for encoded audio delivery"))?
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), VideoTransportError> {
        let result = self
            .request(|reply| Command::Shutdown { reply: Some(reply) })
            .await;
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                VideoTransportError::new(format!("join audio sender actor: {error}"))
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
            .map_err(|_| VideoTransportError::new("audio sender command channel closed"))?;
        response
            .await
            .map_err(|_| VideoTransportError::new("audio sender reply channel closed"))?
    }
}

impl Drop for AudioSenderActor {
    fn drop(&mut self) {
        let _ = self.commands.try_send(Command::Shutdown { reply: None });
        if let Some(task) = self.task.take() {
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
        sender: Box<dyn AudioSenderPort>,
        reply: oneshot::Sender<Result<(), VideoTransportError>>,
    },
    Transition {
        generation: NonZeroU64,
        transition: Transition,
        reply: oneshot::Sender<Result<(), VideoTransportError>>,
    },
    Stop {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<AudioSenderStatistics, VideoTransportError>>,
    },
    Statistics {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<AudioSenderStatistics, VideoTransportError>>,
    },
    Shutdown {
        reply: Option<oneshot::Sender<Result<(), VideoTransportError>>>,
    },
}

struct ActiveSender {
    generation: NonZeroU64,
    state: AudioSenderState,
    sender: Option<Box<dyn AudioSenderPort>>,
    statistics: AudioSenderStatistics,
}

enum Next {
    Command(Option<Command>),
    Packet(Option<EncodedAudioPacket>),
}

async fn run_actor(
    mut commands: mpsc::Receiver<Command>,
    mut output: mpsc::Receiver<EncodedAudioPacket>,
    snapshot: watch::Sender<AudioSenderSnapshot>,
) {
    let mut active: Option<ActiveSender> = None;
    let mut completed_generation = None;
    loop {
        let streaming = active
            .as_ref()
            .is_some_and(|current| current.state == AudioSenderState::Streaming);
        let next = if streaming {
            tokio::select! {
                biased;
                command = commands.recv() => Next::Command(command),
                packet = output.recv() => Next::Packet(packet),
            }
        } else {
            Next::Command(commands.recv().await)
        };
        let command = match next {
            Next::Command(command) => command,
            Next::Packet(packet) => {
                let result = match packet {
                    Some(packet) => forward_packet(&mut active, packet, &snapshot).await,
                    None => Err(VideoTransportError::new(
                        "encoded-audio source channel closed",
                    )),
                };
                if let Err(error) = result {
                    fail_active(&mut active, &snapshot, error).await;
                }
                continue;
            }
        };
        let Some(command) = command else {
            let _ = shutdown_active(&mut active).await;
            publish(
                &snapshot,
                completed_generation,
                AudioSenderState::Stopped,
                AudioSenderStatistics::default(),
                None,
            );
            return;
        };
        match command {
            Command::Configure {
                generation,
                sender,
                reply,
            } => {
                let result = configure_active(
                    &mut active,
                    &mut output,
                    completed_generation,
                    generation,
                    sender,
                    &snapshot,
                )
                .await;
                let _ = reply.send(result);
            }
            Command::Transition {
                generation,
                transition,
                reply,
            } => {
                let result = transition_active(&mut active, generation, transition, &snapshot);
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
                    AudioSenderState::Empty,
                    statistics,
                    result.as_ref().err().map(ToString::to_string),
                );
                let _ = reply.send(result);
            }
            Command::Statistics { generation, reply } => {
                let _ = reply.send(active_statistics(
                    &active,
                    generation,
                    completed_generation,
                    &snapshot,
                ));
            }
            Command::Shutdown { reply } => {
                let result = shutdown_active(&mut active).await;
                publish(
                    &snapshot,
                    completed_generation,
                    AudioSenderState::Stopped,
                    AudioSenderStatistics::default(),
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
    output: &mut mpsc::Receiver<EncodedAudioPacket>,
    completed_generation: Option<NonZeroU64>,
    generation: NonZeroU64,
    sender: Box<dyn AudioSenderPort>,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<(), VideoTransportError> {
    if active.is_some() {
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(
            "an audio sender generation is already active",
        ));
    }
    if completed_generation.is_some_and(|done| generation <= done) {
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(format!(
            "audio sender generation {generation} is not newer than {completed_generation:?}"
        )));
    }
    while output.try_recv().is_ok() {}
    *active = Some(ActiveSender {
        generation,
        state: AudioSenderState::Configured,
        sender: Some(sender),
        statistics: AudioSenderStatistics::default(),
    });
    publish(
        snapshot,
        Some(generation),
        AudioSenderState::Configured,
        AudioSenderStatistics::default(),
        None,
    );
    Ok(())
}

fn transition_active(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
    transition: Transition,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<(), VideoTransportError> {
    let active = matching_active(active, generation)?;
    let (required, desired) = match transition {
        Transition::Start => (AudioSenderState::Configured, AudioSenderState::Streaming),
        Transition::Suspend => (AudioSenderState::Streaming, AudioSenderState::Suspended),
        Transition::Resume => (AudioSenderState::Suspended, AudioSenderState::Streaming),
    };
    if active.state != required {
        return Err(VideoTransportError::new(format!(
            "audio sender generation {generation} is {:?}; expected {required:?}",
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

async fn forward_packet(
    active: &mut Option<ActiveSender>,
    packet: EncodedAudioPacket,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<(), VideoTransportError> {
    let active = active
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("audio sender is missing"))?;
    if packet.media_generation != active.generation {
        active.statistics.dropped_packets = active.statistics.dropped_packets.saturating_add(1);
        return Ok(());
    }
    let bytes = packet.data.len() as u64;
    let queue_delay = Instant::now()
        .checked_duration_since(packet.reference_time)
        .unwrap_or_default();
    let sender = active
        .sender
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("audio sender transport is missing"))?;
    let outcome = tokio::time::timeout(SEND_TIMEOUT, sender.send(packet))
        .await
        .map_err(|_| VideoTransportError::new("timed out enqueueing encoded audio"))??;
    if outcome == AudioSendOutcome::Congested {
        active.statistics.dropped_packets = active.statistics.dropped_packets.saturating_add(1);
    } else {
        active.statistics.packets = active.statistics.packets.saturating_add(1);
        active.statistics.encoded_bytes = active.statistics.encoded_bytes.saturating_add(bytes);
    }
    active.statistics.queue_delay = queue_delay;
    publish(
        snapshot,
        Some(active.generation),
        active.state,
        active.statistics.clone(),
        None,
    );
    Ok(())
}

async fn fail_active(
    active: &mut Option<ActiveSender>,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
    error: VideoTransportError,
) {
    let Some(active) = active.as_mut() else {
        return;
    };
    active.state = AudioSenderState::Failed;
    if let Some(sender) = active.sender.take() {
        let _ = sender.shutdown().await;
    }
    publish(
        snapshot,
        Some(active.generation),
        active.state,
        active.statistics.clone(),
        Some(error.to_string()),
    );
}

async fn stop_active(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
    completed_generation: Option<NonZeroU64>,
) -> Result<AudioSenderStatistics, VideoTransportError> {
    let Some(current) = active.as_ref() else {
        if completed_generation == Some(generation) {
            return Ok(AudioSenderStatistics::default());
        }
        return Err(VideoTransportError::new(
            "there is no matching audio sender generation to stop",
        ));
    };
    if current.generation != generation {
        return Err(generation_mismatch(current.generation, generation));
    }
    let mut current = active.take().expect("active audio sender checked above");
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
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<AudioSenderStatistics, VideoTransportError> {
    match active {
        Some(active) if active.generation != generation => {
            Err(generation_mismatch(active.generation, generation))
        }
        Some(active) if active.state == AudioSenderState::Failed => Err(VideoTransportError::new(
            snapshot
                .borrow()
                .last_error
                .clone()
                .unwrap_or_else(|| "audio sender failed without diagnostic detail".into()),
        )),
        Some(active) => Ok(active.statistics.clone()),
        None if completed_generation == Some(generation) => {
            Ok(snapshot.borrow().statistics.clone())
        }
        None => Err(VideoTransportError::new(
            "there is no matching audio sender generation for statistics",
        )),
    }
}

fn matching_active(
    active: &mut Option<ActiveSender>,
    generation: NonZeroU64,
) -> Result<&mut ActiveSender, VideoTransportError> {
    let active = active
        .as_mut()
        .ok_or_else(|| VideoTransportError::new("there is no active audio sender generation"))?;
    if active.generation != generation {
        return Err(generation_mismatch(active.generation, generation));
    }
    Ok(active)
}

fn generation_mismatch(active: NonZeroU64, requested: NonZeroU64) -> VideoTransportError {
    VideoTransportError::new(format!(
        "requested audio sender generation {requested}; active generation is {active}"
    ))
}

fn publish(
    snapshot: &watch::Sender<AudioSenderSnapshot>,
    generation: Option<NonZeroU64>,
    state: AudioSenderState,
    statistics: AudioSenderStatistics,
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
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    #[derive(Debug)]
    struct RecordingSender {
        timestamps: Arc<Mutex<Vec<Duration>>>,
    }

    #[async_trait]
    impl AudioSenderPort for RecordingSender {
        async fn send(
            &mut self,
            packet: EncodedAudioPacket,
        ) -> Result<AudioSendOutcome, VideoTransportError> {
            self.timestamps
                .lock()
                .expect("timestamp mutex poisoned")
                .push(packet.media_timestamp);
            Ok(AudioSendOutcome::Accepted)
        }

        async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn generation_scopes_audio_delivery_and_orderly_stop() {
        let (output, receiver) = mpsc::channel(4);
        let actor = AudioSenderActor::spawn(receiver);
        let timestamps = Arc::new(Mutex::new(Vec::new()));
        let generation = NonZeroU64::new(7).unwrap();
        actor
            .configure(
                generation,
                Box::new(RecordingSender {
                    timestamps: timestamps.clone(),
                }),
            )
            .await
            .unwrap();
        actor.start(generation).await.unwrap();
        output.send(packet(generation, 0)).await.unwrap();
        actor
            .wait_for_packet_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(timestamps.lock().unwrap().as_slice(), [Duration::ZERO]);
        let statistics = actor.stop(generation).await.unwrap();
        assert_eq!(statistics.packets, 1);
        actor.shutdown().await.unwrap();
    }

    fn packet(generation: NonZeroU64, timestamp_ms: u64) -> EncodedAudioPacket {
        EncodedAudioPacket {
            media_generation: generation,
            data: vec![0xf8, 0xff, 0xfe],
            media_timestamp: Duration::from_millis(timestamp_ms),
            reference_time: Instant::now(),
            duration: Duration::from_millis(20),
        }
    }
}
