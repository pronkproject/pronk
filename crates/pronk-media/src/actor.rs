use std::num::NonZeroU64;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::gstreamer_graph::GStreamerGraph;
use crate::model::{
    EncodedAudioPacket, EncodedVideoAccessUnit, MediaGraphConfiguration, MediaGraphError,
    MediaGraphSnapshot, MediaGraphState, MediaGraphStatistics, MAX_ENCODED_OUTPUT_CAPACITY,
};

const COMMAND_CAPACITY: usize = 8;

pub struct MediaGraphActor {
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<MediaGraphSnapshot>,
    worker: std::thread::Thread,
    task: Option<JoinHandle<()>>,
}

pub struct EncodedMediaReceivers {
    pub video: mpsc::Receiver<EncodedVideoAccessUnit>,
    pub audio: mpsc::Receiver<EncodedAudioPacket>,
}

#[derive(Clone, Default)]
struct EncodedMediaSenders {
    video: Option<mpsc::Sender<EncodedVideoAccessUnit>>,
    audio: Option<mpsc::Sender<EncodedAudioPacket>>,
}

impl std::fmt::Debug for MediaGraphActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaGraphActor")
            .field("state", &self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl MediaGraphActor {
    pub fn spawn() -> Result<Self, MediaGraphError> {
        Self::spawn_inner(EncodedMediaSenders::default())
    }

    pub fn spawn_with_output(
        capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<EncodedVideoAccessUnit>), MediaGraphError> {
        if !(1..=MAX_ENCODED_OUTPUT_CAPACITY).contains(&capacity) {
            return Err(MediaGraphError::new(format!(
                "encoded-video output capacity must be between 1 and {MAX_ENCODED_OUTPUT_CAPACITY}"
            )));
        }
        let (output, receiver) = mpsc::channel(capacity);
        Self::spawn_inner(EncodedMediaSenders {
            video: Some(output),
            audio: None,
        })
        .map(|actor| (actor, receiver))
    }

    pub fn spawn_with_media_output(
        video_capacity: usize,
        audio_capacity: usize,
    ) -> Result<(Self, EncodedMediaReceivers), MediaGraphError> {
        validate_output_capacity("encoded-video", video_capacity)?;
        validate_output_capacity("encoded-audio", audio_capacity)?;
        let (video, video_receiver) = mpsc::channel(video_capacity);
        let (audio, audio_receiver) = mpsc::channel(audio_capacity);
        Self::spawn_inner(EncodedMediaSenders {
            video: Some(video),
            audio: Some(audio),
        })
        .map(|actor| {
            (
                actor,
                EncodedMediaReceivers {
                    video: video_receiver,
                    audio: audio_receiver,
                },
            )
        })
    }

    fn spawn_inner(output: EncodedMediaSenders) -> Result<Self, MediaGraphError> {
        tokio::runtime::Handle::try_current().map_err(|_| {
            MediaGraphError::new("MediaGraphActor requires a running Tokio runtime")
        })?;
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (state_tx, state) = watch::channel(MediaGraphSnapshot::empty());
        let task = std::thread::Builder::new()
            .name("pronk-gstreamer-media".into())
            .spawn(move || run_worker(receiver, state_tx, output))
            .map_err(|error| {
                MediaGraphError::new(format!("spawn GStreamer media actor: {error}"))
            })?;
        let worker = task.thread().clone();
        Ok(Self {
            commands,
            state,
            worker,
            task: Some(task),
        })
    }

    pub fn snapshot(&self) -> MediaGraphSnapshot {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<MediaGraphSnapshot> {
        self.state.clone()
    }

    pub async fn configure(
        &self,
        configuration: MediaGraphConfiguration,
    ) -> Result<(), MediaGraphError> {
        self.request(|reply| Command::Configure {
            configuration,
            reply,
        })
        .await
    }

    pub async fn start(&self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.request(|reply| Command::Start { generation, reply })
            .await
    }

    pub async fn suspend(&self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.request(|reply| Command::Suspend { generation, reply })
            .await
    }

    pub async fn resume(&self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.request(|reply| Command::Resume { generation, reply })
            .await
    }

    pub async fn request_key_frame(&self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.request(|reply| Command::RequestKeyFrame { generation, reply })
            .await
    }

    pub async fn set_video_bitrate(
        &self,
        generation: NonZeroU64,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        self.request(|reply| Command::SetVideoBitrate {
            generation,
            bitrate,
            reply,
        })
        .await
    }

    pub async fn stop(
        &self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.request(|reply| Command::Stop { generation, reply })
            .await
    }

    pub async fn statistics(
        &self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.request(|reply| Command::Statistics { generation, reply })
            .await
    }

    pub async fn shutdown(mut self) -> Result<(), MediaGraphError> {
        let result = self
            .request(|reply| Command::Shutdown { reply: Some(reply) })
            .await;
        if let Some(task) = self.task.take() {
            tokio::task::spawn_blocking(move || task.join())
                .await
                .map_err(|error| MediaGraphError::new(format!("join media actor: {error}")))?
                .map_err(|_| MediaGraphError::new("GStreamer media actor panicked"))?;
        }
        result
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, MediaGraphError>>) -> Command,
    ) -> Result<T, MediaGraphError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| MediaGraphError::new("GStreamer media actor command channel closed"))?;
        self.worker.unpark();
        response
            .await
            .map_err(|_| MediaGraphError::new("GStreamer media actor reply channel closed"))?
    }
}

fn validate_output_capacity(label: &str, capacity: usize) -> Result<(), MediaGraphError> {
    if !(1..=MAX_ENCODED_OUTPUT_CAPACITY).contains(&capacity) {
        return Err(MediaGraphError::new(format!(
            "{label} output capacity must be between 1 and {MAX_ENCODED_OUTPUT_CAPACITY}"
        )));
    }
    Ok(())
}

impl Drop for MediaGraphActor {
    fn drop(&mut self) {
        let _ = self.commands.try_send(Command::Shutdown { reply: None });
        self.worker.unpark();
        // Closing the last sender is also a shutdown signal. The worker owns
        // and tears down the thread-affine graph before it exits.
        self.task.take();
    }
}

enum Command {
    Configure {
        configuration: MediaGraphConfiguration,
        reply: oneshot::Sender<Result<(), MediaGraphError>>,
    },
    Start {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), MediaGraphError>>,
    },
    Suspend {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), MediaGraphError>>,
    },
    Resume {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), MediaGraphError>>,
    },
    RequestKeyFrame {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<(), MediaGraphError>>,
    },
    SetVideoBitrate {
        generation: NonZeroU64,
        bitrate: NonZeroU64,
        reply: oneshot::Sender<Result<u64, MediaGraphError>>,
    },
    Stop {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<MediaGraphStatistics, MediaGraphError>>,
    },
    Statistics {
        generation: NonZeroU64,
        reply: oneshot::Sender<Result<MediaGraphStatistics, MediaGraphError>>,
    },
    Shutdown {
        reply: Option<oneshot::Sender<Result<(), MediaGraphError>>>,
    },
}

struct ActiveGraph {
    generation: NonZeroU64,
    state: MediaGraphState,
    graph: GStreamerGraph,
}

fn run_worker(
    mut commands: mpsc::Receiver<Command>,
    state: watch::Sender<MediaGraphSnapshot>,
    output: EncodedMediaSenders,
) {
    let mut active: Option<ActiveGraph> = None;
    let mut completed_generation = None;

    loop {
        let command = if active.is_some() {
            match commands.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::error::TryRecvError::Disconnected) => None,
                Err(mpsc::error::TryRecvError::Empty) => {
                    if let Some(current) = active.as_mut() {
                        if let Err(error) = current.graph.poll(Duration::ZERO) {
                            let generation = current.generation;
                            let statistics = current.graph.statistics();
                            let failed = active.take().expect("active graph was borrowed above");
                            let _ = failed.graph.stop();
                            completed_generation = Some(generation);
                            publish(
                                &state,
                                Some(generation),
                                MediaGraphState::Failed,
                                statistics,
                                Some(error.to_string()),
                            );
                        }
                    }
                    // AppSink callbacks, fatal bus messages, and commands all
                    // unpark this owner thread. An unpark racing this call
                    // leaves a token, so no readiness transition is lost.
                    if active.is_some() {
                        std::thread::park();
                    }
                    continue;
                }
            }
        } else {
            commands.blocking_recv()
        };

        let Some(command) = command else {
            if let Some(current) = active.take() {
                let _ = current.graph.stop();
            }
            publish(
                &state,
                completed_generation,
                MediaGraphState::Stopped,
                MediaGraphStatistics::default(),
                None,
            );
            return;
        };

        match command {
            Command::Configure {
                configuration,
                reply,
            } => {
                let requested = configuration.media_generation;
                let result = if let Some(current) = active.as_ref() {
                    Err(MediaGraphError::new(format!(
                        "media generation {} is still {:?}; cannot configure {requested}",
                        current.generation, current.state
                    )))
                } else if completed_generation.is_some_and(|previous| requested <= previous) {
                    Err(MediaGraphError::new(format!(
                        "media generation {requested} is not newer than completed generation {completed_generation:?}"
                    )))
                } else {
                    match GStreamerGraph::configure(
                        configuration,
                        output.video.clone(),
                        output.audio.clone(),
                    ) {
                        Ok(graph) => {
                            active = Some(ActiveGraph {
                                generation: requested,
                                state: MediaGraphState::Configured,
                                graph,
                            });
                            publish(
                                &state,
                                Some(requested),
                                MediaGraphState::Configured,
                                MediaGraphStatistics::default(),
                                None,
                            );
                            Ok(())
                        }
                        Err(error) => {
                            completed_generation = Some(requested);
                            publish(
                                &state,
                                Some(requested),
                                MediaGraphState::Failed,
                                MediaGraphStatistics::default(),
                                Some(error.to_string()),
                            );
                            Err(error)
                        }
                    }
                };
                let _ = reply.send(result);
            }
            Command::Start { generation, reply } => {
                let result = with_active(&mut active, generation, MediaGraphState::Configured)
                    .and_then(|current| {
                        current.graph.start()?;
                        current.state = MediaGraphState::Streaming;
                        publish(
                            &state,
                            Some(generation),
                            current.state,
                            current.graph.statistics(),
                            None,
                        );
                        Ok(())
                    });
                let _ = reply.send(result);
            }
            Command::Suspend { generation, reply } => {
                let result = with_active(&mut active, generation, MediaGraphState::Streaming)
                    .and_then(|current| {
                        current.graph.suspend()?;
                        current.state = MediaGraphState::Suspended;
                        publish(
                            &state,
                            Some(generation),
                            current.state,
                            current.graph.statistics(),
                            None,
                        );
                        Ok(())
                    });
                let _ = reply.send(result);
            }
            Command::Resume { generation, reply } => {
                let result = with_active(&mut active, generation, MediaGraphState::Suspended)
                    .and_then(|current| {
                        current.graph.resume()?;
                        current.state = MediaGraphState::Streaming;
                        publish(
                            &state,
                            Some(generation),
                            current.state,
                            current.graph.statistics(),
                            None,
                        );
                        Ok(())
                    });
                let _ = reply.send(result);
            }
            Command::RequestKeyFrame { generation, reply } => {
                let result = matching_active(&mut active, generation)
                    .and_then(|current| current.graph.request_key_frame());
                if let Some(current) = active.as_ref() {
                    publish(
                        &state,
                        Some(current.generation),
                        current.state,
                        current.graph.statistics(),
                        result.as_ref().err().map(ToString::to_string),
                    );
                }
                let _ = reply.send(result);
            }
            Command::SetVideoBitrate {
                generation,
                bitrate,
                reply,
            } => {
                let result = matching_active(&mut active, generation)
                    .and_then(|current| current.graph.set_video_bitrate(bitrate));
                if let Some(current) = active.as_ref() {
                    publish(
                        &state,
                        Some(current.generation),
                        current.state,
                        current.graph.statistics(),
                        result.as_ref().err().map(ToString::to_string),
                    );
                }
                let _ = reply.send(result);
            }
            Command::Stop { generation, reply } => {
                let result = match active.as_ref() {
                    Some(current) if current.generation != generation => {
                        Err(generation_mismatch(current.generation, generation))
                    }
                    Some(_) => {
                        let current = active.take().expect("active generation checked");
                        let result = current.graph.stop();
                        completed_generation = Some(generation);
                        let statistics = result.as_ref().cloned().unwrap_or_default();
                        publish(
                            &state,
                            Some(generation),
                            MediaGraphState::Empty,
                            statistics,
                            result.as_ref().err().map(ToString::to_string),
                        );
                        result
                    }
                    None if completed_generation == Some(generation) => {
                        let statistics = state.borrow().statistics.clone();
                        publish(
                            &state,
                            Some(generation),
                            MediaGraphState::Empty,
                            statistics.clone(),
                            None,
                        );
                        Ok(statistics)
                    }
                    None => Err(MediaGraphError::new(
                        "there is no matching media generation to stop",
                    )),
                };
                let _ = reply.send(result);
            }
            Command::Statistics { generation, reply } => {
                let result = match active.as_mut() {
                    Some(current) if current.generation == generation => current
                        .graph
                        .poll(Duration::ZERO)
                        .map(|()| current.graph.statistics()),
                    Some(current) => Err(generation_mismatch(current.generation, generation)),
                    None if completed_generation == Some(generation) => {
                        let snapshot = state.borrow().clone();
                        if snapshot.state == MediaGraphState::Failed {
                            Err(MediaGraphError::new(snapshot.last_error.unwrap_or_else(
                                || "GStreamer media graph failed without diagnostic detail".into(),
                            )))
                        } else {
                            Ok(snapshot.statistics)
                        }
                    }
                    None => Err(MediaGraphError::new(
                        "there is no matching media generation for statistics",
                    )),
                };
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                let result = match active.take() {
                    Some(current) => current.graph.stop().map(|_| ()),
                    None => Ok(()),
                };
                publish(
                    &state,
                    completed_generation,
                    MediaGraphState::Stopped,
                    MediaGraphStatistics::default(),
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

fn with_active(
    active: &mut Option<ActiveGraph>,
    requested: NonZeroU64,
    required: MediaGraphState,
) -> Result<&mut ActiveGraph, MediaGraphError> {
    let current = active
        .as_mut()
        .ok_or_else(|| MediaGraphError::new("there is no active media generation"))?;
    if current.generation != requested {
        return Err(generation_mismatch(current.generation, requested));
    }
    if current.state != required {
        return Err(MediaGraphError::new(format!(
            "media generation {requested} is {:?}; expected {required:?}",
            current.state
        )));
    }
    Ok(current)
}

fn matching_active(
    active: &mut Option<ActiveGraph>,
    requested: NonZeroU64,
) -> Result<&mut ActiveGraph, MediaGraphError> {
    let current = active
        .as_mut()
        .ok_or_else(|| MediaGraphError::new("there is no active media generation"))?;
    if current.generation != requested {
        return Err(generation_mismatch(current.generation, requested));
    }
    Ok(current)
}

fn generation_mismatch(active: NonZeroU64, requested: NonZeroU64) -> MediaGraphError {
    MediaGraphError::new(format!(
        "requested media generation {requested}; active generation is {active}"
    ))
}

fn publish(
    state: &watch::Sender<MediaGraphSnapshot>,
    generation: Option<NonZeroU64>,
    graph_state: MediaGraphState,
    statistics: MediaGraphStatistics,
    last_error: Option<String>,
) {
    state.send_modify(|snapshot| {
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.media_generation = generation;
        snapshot.state = graph_state;
        snapshot.statistics = statistics;
        snapshot.last_error = last_error;
    });
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    use crate::{
        MediaGraphConfiguration, MediaGraphState, PipeWireVideoInput, MAX_ENCODED_OUTPUT_CAPACITY,
    };

    use super::MediaGraphActor;

    #[tokio::test(flavor = "current_thread")]
    async fn failed_configuration_has_generation_scoped_idempotent_cleanup() {
        let actor = MediaGraphActor::spawn().unwrap();
        let generation = NonZeroU64::new(1).unwrap();
        assert!(actor
            .configure(invalid_configuration(generation))
            .await
            .is_err());
        let failed = actor.snapshot();
        assert_eq!(failed.media_generation, Some(generation));
        assert_eq!(failed.state, MediaGraphState::Failed);
        assert!(failed.last_error.is_some());
        assert!(actor.statistics(generation).await.is_err());

        actor.stop(generation).await.unwrap();
        actor.stop(generation).await.unwrap();
        assert_eq!(actor.snapshot().state, MediaGraphState::Empty);
        assert!(actor
            .configure(invalid_configuration(generation))
            .await
            .is_err());
        actor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn encoded_output_capacity_is_explicitly_bounded() {
        assert!(MediaGraphActor::spawn_with_output(0).is_err());
        assert!(MediaGraphActor::spawn_with_output(MAX_ENCODED_OUTPUT_CAPACITY + 1).is_err());

        let (actor, output) = MediaGraphActor::spawn_with_output(1).unwrap();
        assert_eq!(output.max_capacity(), 1);
        actor.shutdown().await.unwrap();

        assert!(MediaGraphActor::spawn_with_media_output(0, 1).is_err());
        assert!(MediaGraphActor::spawn_with_media_output(1, 0).is_err());
        let (actor, outputs) = MediaGraphActor::spawn_with_media_output(2, 3).unwrap();
        assert_eq!(outputs.video.max_capacity(), 2);
        assert_eq!(outputs.audio.max_capacity(), 3);
        actor.shutdown().await.unwrap();
    }

    fn invalid_configuration(generation: NonZeroU64) -> MediaGraphConfiguration {
        let (remote, peer) = UnixStream::pair().unwrap();
        drop(peer);
        MediaGraphConfiguration {
            media_generation: generation,
            video: PipeWireVideoInput {
                remote: OwnedFd::from(remote),
                node_name: "pronk.test.invalid".into(),
                object_serial: NonZeroU64::new(1).unwrap(),
                caps: "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1".into(),
            },
            audio: None,
            video_bitrate: NonZeroU64::new(1_000_000).unwrap(),
        }
    }
}
