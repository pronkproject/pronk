use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use pronk_media::EncodedAudioPacket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::generation_slot::{GenerationOwned, GenerationSlot};
use crate::sender_status::{SenderState as AudioSenderState, SenderStatus as AudioSenderStatus};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AudioSenderSnapshot {
    status: AudioSenderStatus,
    statistics: AudioSenderStatistics,
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
            status: AudioSenderStatus::Empty,
            statistics: AudioSenderStatistics::default(),
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
                if current.status.generation() != Some(generation) {
                    return Err(VideoTransportError::new(format!(
                        "audio sender generation changed while waiting for {generation}"
                    )));
                }
                if let AudioSenderStatus::Failed { error, .. } = current.status {
                    return Err(VideoTransportError::new(error));
                }
                if current.statistics.packets > previous {
                    return Ok(());
                }
                if matches!(
                    current.status,
                    AudioSenderStatus::Empty
                        | AudioSenderStatus::Completed { .. }
                        | AudioSenderStatus::Stopped { .. }
                ) {
                    return Err(VideoTransportError::new(format!(
                        "audio sender generation {generation} stopped before encoded audio delivery"
                    )));
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
        // Channel closure also wakes a full queue. The actor keeps the
        // transport until its shutdown command or closure is handled.
        self.task.take();
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
    phase: AudioPhase,
    statistics: AudioSenderStatistics,
}

enum AudioPhase {
    Configured(Box<dyn AudioSenderPort>),
    Streaming(Box<dyn AudioSenderPort>),
    Suspended(Box<dyn AudioSenderPort>),
    Failed,
}

impl AudioPhase {
    fn state(&self) -> AudioSenderState {
        match self {
            Self::Configured(_) => AudioSenderState::Configured,
            Self::Streaming(_) => AudioSenderState::Streaming,
            Self::Suspended(_) => AudioSenderState::Suspended,
            Self::Failed => AudioSenderState::Failed,
        }
    }

    fn take_sender(&mut self) -> Option<Box<dyn AudioSenderPort>> {
        match std::mem::replace(self, Self::Failed) {
            Self::Configured(sender) | Self::Streaming(sender) | Self::Suspended(sender) => {
                Some(sender)
            }
            Self::Failed => None,
        }
    }
}

impl GenerationOwned for ActiveSender {
    fn generation(&self) -> NonZeroU64 {
        self.generation
    }
}

type SenderSlot = GenerationSlot<ActiveSender>;

enum Next {
    Command(Option<Command>),
    Packet(Option<EncodedAudioPacket>),
}

async fn run_actor(
    mut commands: mpsc::Receiver<Command>,
    mut output: mpsc::Receiver<EncodedAudioPacket>,
    snapshot: watch::Sender<AudioSenderSnapshot>,
) {
    let mut active = SenderSlot::empty();
    loop {
        let streaming = active
            .active()
            .is_some_and(|current| current.phase.state() == AudioSenderState::Streaming);
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
                AudioSenderStatus::Stopped {
                    generation: active.completed(),
                    error: None,
                },
                AudioSenderStatistics::default(),
            );
            return;
        };
        match command {
            Command::Configure {
                generation,
                sender,
                reply,
            } => {
                let result =
                    configure_active(&mut active, &mut output, generation, sender, &snapshot).await;
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
                let stopping_active = active
                    .active()
                    .is_some_and(|current| current.generation == generation);
                let previous_statistics = snapshot.borrow().statistics.clone();
                let result = stop_active(&mut active, generation).await;
                if stopping_active || result.is_ok() {
                    let statistics = if stopping_active {
                        result.as_ref().cloned().unwrap_or(previous_statistics)
                    } else {
                        previous_statistics
                    };
                    publish(
                        &snapshot,
                        AudioSenderStatus::Completed {
                            generation,
                            error: result.as_ref().err().map(ToString::to_string),
                        },
                        statistics,
                    );
                }
                let _ = reply.send(result);
            }
            Command::Statistics { generation, reply } => {
                let _ = reply.send(active_statistics(&active, generation, &snapshot));
            }
            Command::Shutdown { reply } => {
                let result = shutdown_active(&mut active).await;
                publish(
                    &snapshot,
                    AudioSenderStatus::Stopped {
                        generation: active.completed(),
                        error: result.as_ref().err().map(ToString::to_string),
                    },
                    AudioSenderStatistics::default(),
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
    active: &mut SenderSlot,
    output: &mut mpsc::Receiver<EncodedAudioPacket>,
    generation: NonZeroU64,
    sender: Box<dyn AudioSenderPort>,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<(), VideoTransportError> {
    if active.active().is_some() {
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(
            "an audio sender generation is already active",
        ));
    }
    let completed_generation = active.completed();
    if completed_generation.is_some_and(|done| generation <= done) {
        let _ = sender.shutdown().await;
        return Err(VideoTransportError::new(format!(
            "audio sender generation {generation} is not newer than {completed_generation:?}"
        )));
    }
    while output.try_recv().is_ok() {}
    *active = SenderSlot::Active(ActiveSender {
        generation,
        phase: AudioPhase::Configured(sender),
        statistics: AudioSenderStatistics::default(),
    });
    publish(
        snapshot,
        AudioSenderStatus::Configured(generation),
        AudioSenderStatistics::default(),
    );
    Ok(())
}

fn transition_active(
    active: &mut SenderSlot,
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
    if active.phase.state() != required {
        return Err(VideoTransportError::new(format!(
            "audio sender generation {generation} is {:?}; expected {required:?}",
            active.phase.state()
        )));
    }
    let sender = active
        .phase
        .take_sender()
        .expect("validated audio transition owns a transport");
    active.phase = match transition {
        Transition::Start | Transition::Resume => AudioPhase::Streaming(sender),
        Transition::Suspend => AudioPhase::Suspended(sender),
    };
    publish(
        snapshot,
        AudioSenderStatus::active(generation, desired),
        active.statistics.clone(),
    );
    Ok(())
}

async fn forward_packet(
    active: &mut SenderSlot,
    packet: EncodedAudioPacket,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<(), VideoTransportError> {
    let active = active
        .active_mut()
        .ok_or_else(|| VideoTransportError::new("audio sender is missing"))?;
    if packet.media_generation != active.generation {
        active.statistics.dropped_packets = active.statistics.dropped_packets.saturating_add(1);
        return Ok(());
    }
    let bytes = packet.data.len() as u64;
    let queue_delay = Instant::now()
        .checked_duration_since(packet.reference_time)
        .unwrap_or_default();
    let AudioPhase::Streaming(sender) = &mut active.phase else {
        return Err(VideoTransportError::new("audio sender is not streaming"));
    };
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
        AudioSenderStatus::active(active.generation, active.phase.state()),
        active.statistics.clone(),
    );
    Ok(())
}

async fn fail_active(
    active: &mut SenderSlot,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
    error: VideoTransportError,
) {
    let Some(active) = active.active_mut() else {
        return;
    };
    if let Some(sender) = active.phase.take_sender() {
        let _ = sender.shutdown().await;
    }
    publish(
        snapshot,
        AudioSenderStatus::Failed {
            generation: active.generation,
            error: error.to_string(),
        },
        active.statistics.clone(),
    );
}

async fn stop_active(
    active: &mut SenderSlot,
    generation: NonZeroU64,
) -> Result<AudioSenderStatistics, VideoTransportError> {
    let Some(current) = active.active() else {
        if active.completed() == Some(generation) {
            return Ok(AudioSenderStatistics::default());
        }
        return Err(VideoTransportError::new(
            "there is no matching audio sender generation to stop",
        ));
    };
    if current.generation != generation {
        return Err(generation_mismatch(current.generation, generation));
    }
    let mut current = active
        .take_active()
        .expect("active audio sender checked above");
    let statistics = current.statistics.clone();
    if let Some(sender) = current.phase.take_sender() {
        sender.shutdown().await?;
    }
    Ok(statistics)
}

async fn shutdown_active(active: &mut SenderSlot) -> Result<(), VideoTransportError> {
    let Some(mut active) = active.take_active() else {
        return Ok(());
    };
    match active.phase.take_sender() {
        Some(sender) => sender.shutdown().await,
        None => Ok(()),
    }
}

fn active_statistics(
    active: &SenderSlot,
    generation: NonZeroU64,
    snapshot: &watch::Sender<AudioSenderSnapshot>,
) -> Result<AudioSenderStatistics, VideoTransportError> {
    match active.active() {
        Some(active) if active.generation != generation => {
            Err(generation_mismatch(active.generation, generation))
        }
        Some(active) if active.phase.state() == AudioSenderState::Failed => match &snapshot
            .borrow()
            .status
        {
            AudioSenderStatus::Failed { error, .. } => Err(VideoTransportError::new(error.clone())),
            _ => unreachable!("failed audio sender has a failed snapshot"),
        },
        Some(active) => Ok(active.statistics.clone()),
        None if active.completed() == Some(generation) => Ok(snapshot.borrow().statistics.clone()),
        None => Err(VideoTransportError::new(
            "there is no matching audio sender generation for statistics",
        )),
    }
}

fn matching_active(
    active: &mut SenderSlot,
    generation: NonZeroU64,
) -> Result<&mut ActiveSender, VideoTransportError> {
    let active = active
        .active_mut()
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
    status: AudioSenderStatus,
    statistics: AudioSenderStatistics,
) {
    let next = AudioSenderSnapshot { status, statistics };
    snapshot.send_if_modified(|current| {
        if *current == next {
            return false;
        }
        *current = next;
        true
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
        fail_shutdown: bool,
        shutdown_signal: Option<oneshot::Sender<()>>,
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
            if let Some(signal) = self.shutdown_signal {
                let _ = signal.send(());
            }
            if self.fail_shutdown {
                Err(VideoTransportError::new("audio sender shutdown failed"))
            } else {
                Ok(())
            }
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
                    fail_shutdown: false,
                    shutdown_signal: None,
                }),
            )
            .await
            .unwrap();
        assert!(actor.resume(generation).await.is_err());
        actor.start(generation).await.unwrap();
        output.send(packet(generation, 0)).await.unwrap();
        actor
            .wait_for_packet_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(timestamps.lock().unwrap().as_slice(), [Duration::ZERO]);
        let statistics = actor.stop(generation).await.unwrap();
        assert_eq!(statistics.packets, 1);
        actor.stop(generation).await.unwrap();
        assert_eq!(actor.snapshot.borrow().statistics.packets, 1);
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stopped_generation_ends_audio_delivery_wait() {
        let (_output, receiver) = mpsc::channel(1);
        let actor = AudioSenderActor::spawn(receiver);
        let generation = NonZeroU64::new(9).unwrap();
        actor
            .configure(
                generation,
                Box::new(RecordingSender {
                    timestamps: Arc::new(Mutex::new(Vec::new())),
                    fail_shutdown: false,
                    shutdown_signal: None,
                }),
            )
            .await
            .unwrap();
        actor.stop(generation).await.unwrap();

        let error = tokio::time::timeout(
            Duration::from_millis(100),
            actor.wait_for_packet_after(generation, 0, Duration::from_secs(2)),
        )
        .await
        .expect("stopped audio sender left a waiter pending")
        .unwrap_err();
        assert!(error.to_string().contains("stopped before"));
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_audio_source_reports_the_same_error_to_waiters_and_statistics() {
        let (output, receiver) = mpsc::channel(1);
        let actor = AudioSenderActor::spawn(receiver);
        let generation = NonZeroU64::new(9).unwrap();
        actor
            .configure(
                generation,
                Box::new(RecordingSender {
                    timestamps: Arc::new(Mutex::new(Vec::new())),
                    fail_shutdown: false,
                    shutdown_signal: None,
                }),
            )
            .await
            .unwrap();
        actor.start(generation).await.unwrap();
        drop(output);

        let wait_error = actor
            .wait_for_packet_after(generation, 0, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(wait_error.to_string().contains("source channel closed"));
        assert_eq!(
            actor.statistics(generation).await.unwrap_err().to_string(),
            wait_error.to_string()
        );
        actor.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejected_stop_preserves_audio_state_and_failed_teardown_consumes_generation() {
        let (_output, receiver) = mpsc::channel(1);
        let actor = AudioSenderActor::spawn(receiver);
        let generation = NonZeroU64::new(7).unwrap();
        let next = NonZeroU64::new(8).unwrap();
        let timestamps = Arc::new(Mutex::new(Vec::new()));
        actor
            .configure(
                generation,
                Box::new(RecordingSender {
                    timestamps: Arc::clone(&timestamps),
                    fail_shutdown: true,
                    shutdown_signal: None,
                }),
            )
            .await
            .unwrap();

        assert!(actor.stop(next).await.is_err());
        assert_eq!(
            actor.snapshot.borrow().status,
            AudioSenderStatus::Configured(generation)
        );
        assert!(actor.stop(generation).await.is_err());
        assert!(matches!(
            actor.snapshot.borrow().status,
            AudioSenderStatus::Completed { generation: completed, error: Some(_) } if completed == generation
        ));
        assert!(actor
            .configure(
                generation,
                Box::new(RecordingSender {
                    timestamps: Arc::clone(&timestamps),
                    fail_shutdown: false,
                    shutdown_signal: None,
                }),
            )
            .await
            .is_err());
        actor.stop(generation).await.unwrap();
        actor
            .configure(
                next,
                Box::new(RecordingSender {
                    timestamps,
                    fail_shutdown: false,
                    shutdown_signal: None,
                }),
            )
            .await
            .unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_audio_actor_finishes_transport_shutdown() {
        let (_output, receiver) = mpsc::channel(1);
        let actor = AudioSenderActor::spawn(receiver);
        let (shutdown_signal, shutdown_done) = oneshot::channel();
        actor
            .configure(
                NonZeroU64::new(1).unwrap(),
                Box::new(RecordingSender {
                    timestamps: Arc::new(Mutex::new(Vec::new())),
                    fail_shutdown: false,
                    shutdown_signal: Some(shutdown_signal),
                }),
            )
            .await
            .unwrap();

        drop(actor);
        tokio::time::timeout(Duration::from_secs(1), shutdown_done)
            .await
            .unwrap()
            .unwrap();
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
