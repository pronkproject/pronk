use std::fmt::Debug;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd as StdOwnedFd;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pronk_backend_protocol::{
    validate_media_configuration, DeviceCapabilities, MediaConfiguration, MediaKind,
    PipeWireTarget, SessionState, SessionStatistics, Validate, SESSION_FEATURE_AUDIO,
};
use pronk_media::{
    EncodedAudioPacket, EncodedMediaReceivers, EncodedVideoAccessUnit, MediaGraphActor,
    MediaGraphConfiguration, MediaGraphError, MediaGraphStatistics, PipeWireAudioInput,
    PipeWireVideoInput, ValidatedAudioCaps, ValidatedVideoCaps, OPUS_BITRATE, OPUS_CHANNELS,
    OPUS_FRAME_DURATION, OPUS_SAMPLE_RATE,
};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use zbus::zvariant::OwnedFd;

use crate::audio_sender_actor::{AudioSenderActor, AudioSenderStatistics};
use crate::feedback::{
    AdaptivePlayoutDelayConfiguration, VideoFeedbackAction, VideoFeedbackController,
    INITIAL_PLAYOUT_DELAY, MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS,
};
use crate::sender_actor::{VideoSenderActor, VideoSenderFeedbackSnapshot, VideoSenderStatistics};
use crate::transport::{
    AudioTransportConfiguration, VideoTransportConfiguration, VideoTransportError,
    VideoTransportNegotiator,
};

const ENCODED_OUTPUT_CAPACITY: usize = 8;
const ENCODED_AUDIO_OUTPUT_CAPACITY: usize = 32;
const START_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10);

fn minimum_playout_delay(
    framerate_numerator: u32,
    framerate_denominator: u32,
    audio_enabled: bool,
) -> Duration {
    let frame_milliseconds = u64::from(framerate_denominator)
        .saturating_mul(1_000)
        .div_ceil(u64::from(framerate_numerator))
        .max(1);
    let audio_packet_duration = if audio_enabled {
        OPUS_FRAME_DURATION
    } else {
        Duration::default()
    };
    Duration::from_millis(frame_milliseconds).max(audio_packet_duration)
}

#[async_trait]
trait MediaGraphPort: Debug + Send + 'static {
    async fn configure(
        &mut self,
        configuration: MediaGraphConfiguration,
    ) -> Result<(), MediaGraphError>;
    async fn start(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn suspend(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn resume(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn request_key_frame(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError>;
    async fn set_video_bitrate(
        &mut self,
        generation: NonZeroU64,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError>;
    async fn stop(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError>;
    async fn statistics(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError>;
    async fn shutdown(&mut self) -> Result<(), MediaGraphError>;
}

#[derive(Debug)]
struct GStreamerMediaGraph {
    actor: Option<MediaGraphActor>,
}

impl GStreamerMediaGraph {
    fn spawn() -> Result<(Self, EncodedMediaReceivers), MediaGraphError> {
        let (actor, outputs) = MediaGraphActor::spawn_with_media_output(
            ENCODED_OUTPUT_CAPACITY,
            ENCODED_AUDIO_OUTPUT_CAPACITY,
        )?;
        Ok((Self { actor: Some(actor) }, outputs))
    }

    fn actor(&self) -> Result<&MediaGraphActor, MediaGraphError> {
        self.actor
            .as_ref()
            .ok_or_else(|| MediaGraphError::new("Chromiacast media graph is shut down"))
    }
}

#[async_trait]
impl MediaGraphPort for GStreamerMediaGraph {
    async fn configure(
        &mut self,
        configuration: MediaGraphConfiguration,
    ) -> Result<(), MediaGraphError> {
        self.actor()?.configure(configuration).await
    }

    async fn start(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.start(generation).await
    }

    async fn suspend(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.suspend(generation).await
    }

    async fn resume(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.resume(generation).await
    }

    async fn request_key_frame(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        self.actor()?.request_key_frame(generation).await
    }

    async fn set_video_bitrate(
        &mut self,
        generation: NonZeroU64,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        self.actor()?.set_video_bitrate(generation, bitrate).await
    }

    async fn stop(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.actor()?.stop(generation).await
    }

    async fn statistics(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.actor()?.statistics(generation).await
    }

    async fn shutdown(&mut self) -> Result<(), MediaGraphError> {
        let Some(actor) = self.actor.take() else {
            return Ok(());
        };
        actor.shutdown().await
    }
}

/// Session-local media state machine. It owns generation admission and the
/// backend media graph but has no D-Bus, discovery, or Cast control concerns.
#[derive(Debug)]
pub(crate) struct ChromiacastMediaSession {
    session_id: String,
    session_generation: u64,
    state: SessionState,
    capabilities: Option<DeviceCapabilities>,
    active_generation: Option<NonZeroU64>,
    completed_generation: Option<NonZeroU64>,
    graph_received_generation: bool,
    sender_received_generation: bool,
    audio_sender_received_generation: bool,
    transport_active: bool,
    audio_enabled: bool,
    media_ready: bool,
    video_bitrate: u64,
    feedback_controller: Option<VideoFeedbackController>,
    graph: Box<dyn MediaGraphPort>,
    sender: Option<VideoSenderActor>,
    audio_sender: Option<AudioSenderActor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MediaSessionEvent {
    KeyFrameRequested {
        session_generation: u64,
        media_generation: u64,
    },
    BitrateRequested {
        session_generation: u64,
        media_generation: u64,
        bitrate: u64,
    },
}

impl ChromiacastMediaSession {
    pub(crate) fn spawn(
        session_id: String,
        session_generation: u64,
    ) -> Result<Self, MediaSessionError> {
        let (graph, outputs) = GStreamerMediaGraph::spawn()?;
        Ok(Self::with_graph_outputs(
            session_id,
            session_generation,
            Box::new(graph),
            outputs.video,
            outputs.audio,
        ))
    }

    fn with_graph_outputs(
        session_id: String,
        session_generation: u64,
        graph: Box<dyn MediaGraphPort>,
        video_output: mpsc::Receiver<EncodedVideoAccessUnit>,
        audio_output: mpsc::Receiver<EncodedAudioPacket>,
    ) -> Self {
        Self {
            session_id,
            session_generation,
            state: SessionState::Created,
            capabilities: None,
            active_generation: None,
            completed_generation: None,
            graph_received_generation: false,
            sender_received_generation: false,
            audio_sender_received_generation: false,
            transport_active: false,
            audio_enabled: false,
            media_ready: false,
            video_bitrate: 0,
            feedback_controller: None,
            graph,
            sender: Some(VideoSenderActor::spawn(video_output)),
            audio_sender: Some(AudioSenderActor::spawn(audio_output)),
        }
    }

    #[cfg(test)]
    fn with_graph(
        session_id: String,
        session_generation: u64,
        graph: Box<dyn MediaGraphPort>,
        video_output: mpsc::Receiver<EncodedVideoAccessUnit>,
    ) -> Self {
        let (_audio_output, audio_receiver) = mpsc::channel(1);
        Self::with_graph_outputs(
            session_id,
            session_generation,
            graph,
            video_output,
            audio_receiver,
        )
    }

    pub(crate) fn subscribe_feedback(
        &self,
    ) -> Result<watch::Receiver<VideoSenderFeedbackSnapshot>, MediaSessionError> {
        Ok(self.sender()?.subscribe_feedback())
    }

    pub(crate) fn is_prepared(&self) -> bool {
        self.state != SessionState::Created
    }

    pub(crate) fn session_generation(&self) -> u64 {
        self.session_generation
    }

    pub(crate) fn complete_preparation(
        &mut self,
        capabilities: DeviceCapabilities,
    ) -> Result<(), MediaSessionError> {
        if self.state != SessionState::Created {
            return Err(MediaSessionError::Transition {
                operation: "Prepare",
                state: self.state,
            });
        }
        capabilities
            .validate()
            .map_err(|error| MediaSessionError::InvalidRequest(error.to_string()))?;
        self.capabilities = Some(capabilities);
        self.state = SessionState::Prepared;
        Ok(())
    }

    pub(crate) async fn configure<T: VideoTransportNegotiator + ?Sized>(
        &mut self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: u64,
        transport: &mut T,
    ) -> Result<(), MediaSessionError> {
        validate_media_configuration(remotes.len(), &targets, &configuration, media_generation)
            .map_err(|error| MediaSessionError::InvalidRequest(error.to_string()))?;
        if self.state != SessionState::Prepared {
            return Err(MediaSessionError::Transition {
                operation: "ConfigureMedia",
                state: self.state,
            });
        }
        let generation = NonZeroU64::new(media_generation).ok_or_else(|| {
            MediaSessionError::InvalidRequest("media generation must be nonzero".into())
        })?;
        if self
            .completed_generation
            .is_some_and(|completed| generation <= completed)
        {
            return Err(MediaSessionError::InvalidRequest(format!(
                "media generation {generation} is not newer than completed generation {:?}",
                self.completed_generation
            )));
        }
        let (graph_configuration, transport_configuration) =
            self.graph_configuration(remotes, targets, configuration, generation)?;
        let audio_enabled = graph_configuration.audio.is_some();

        // Admit ownership before crossing the asynchronous graph boundary.
        // Once the method has consumed its passed fd, matching StopMedia must
        // remain valid even if graph setup fails or the D-Bus reply is lost.
        self.active_generation = Some(generation);
        self.graph_received_generation = true;
        self.sender_received_generation = false;
        self.audio_sender_received_generation = false;
        self.transport_active = false;
        self.audio_enabled = audio_enabled;
        self.media_ready = false;
        self.video_bitrate = graph_configuration.video_bitrate.get();
        self.state = SessionState::Configured;
        self.graph.configure(graph_configuration).await?;

        let mut negotiated = transport.negotiate_video(transport_configuration).await?;
        self.transport_active = true;
        let adaptive_playout_delay = negotiated
            .sender
            .supports_target_playout_delay_updates()
            .then(|| AdaptivePlayoutDelayConfiguration {
                minimum: minimum_playout_delay(
                    transport_configuration.framerate_numerator,
                    transport_configuration.framerate_denominator,
                    transport_configuration.audio.is_some(),
                ),
                initial: transport_configuration.target_playout_delay,
                receiver_maximum: negotiated.sender.maximum_target_playout_delay(),
            });
        tracing::info!(
            supported = adaptive_playout_delay.is_some(),
            minimum_milliseconds = adaptive_playout_delay
                .map(|configuration| configuration.minimum.as_millis()),
            initial_milliseconds = transport_configuration.target_playout_delay.as_millis(),
            receiver_maximum_milliseconds = ?adaptive_playout_delay
                .and_then(|configuration| configuration.receiver_maximum)
                .map(|delay| delay.as_millis()),
            "negotiated adaptive Cast playout delay"
        );
        self.feedback_controller = Some(VideoFeedbackController::new(
            NonZeroU64::new(self.video_bitrate).expect("validated bitrate is nonzero"),
            negotiated.minimum_bitrate,
            adaptive_playout_delay,
        ));

        match (audio_enabled, negotiated.audio_sender.take()) {
            (true, Some(audio_sender)) => {
                if let Err(error) = self
                    .audio_sender()?
                    .configure(generation, audio_sender)
                    .await
                {
                    let _ = negotiated.sender.shutdown().await;
                    return Err(error.into());
                }
                self.audio_sender_received_generation = true;
            }
            (true, None) => {
                let _ = negotiated.sender.shutdown().await;
                return Err(MediaSessionError::Transport(
                    "Cast receiver did not provide the negotiated audio sender".into(),
                ));
            }
            (false, Some(audio_sender)) => {
                let _ = audio_sender.shutdown().await;
                let _ = negotiated.sender.shutdown().await;
                return Err(MediaSessionError::Transport(
                    "Cast transport provided audio for a video-only generation".into(),
                ));
            }
            (false, None) => {}
        }
        self.sender()?.configure(generation, negotiated).await?;
        self.sender_received_generation = true;
        self.media_ready = true;
        Ok(())
    }

    pub(crate) async fn handle_feedback(
        &mut self,
        feedback: VideoSenderFeedbackSnapshot,
    ) -> Result<Vec<MediaSessionEvent>, MediaSessionError> {
        let Some(generation) = feedback.generation else {
            return Ok(Vec::new());
        };
        if self.active_generation != Some(generation) || !self.media_ready {
            return Ok(Vec::new());
        }
        if let Some(error) = feedback.terminal_error {
            return Err(error.into());
        }
        let Some(controller) = self.feedback_controller.as_mut() else {
            return Ok(Vec::new());
        };
        let actions = controller.observe(feedback, Instant::now());
        let mut events = Vec::with_capacity(actions.len());
        for action in actions {
            match action {
                VideoFeedbackAction::ForceKeyFrame => {
                    self.graph.request_key_frame(generation).await?;
                    events.push(MediaSessionEvent::KeyFrameRequested {
                        session_generation: self.session_generation,
                        media_generation: generation.get(),
                    });
                }
                VideoFeedbackAction::SetBitrate(bitrate) => {
                    let bitrate = self.graph.set_video_bitrate(generation, bitrate).await?;
                    self.video_bitrate = bitrate;
                    events.push(MediaSessionEvent::BitrateRequested {
                        session_generation: self.session_generation,
                        media_generation: generation.get(),
                        bitrate,
                    });
                }
                VideoFeedbackAction::SetPlayoutDelay(delay) => {
                    self.sender()?
                        .set_target_playout_delay(generation, delay)
                        .await?;
                    tracing::info!(
                        media_generation = generation.get(),
                        milliseconds = delay.as_millis(),
                        "adjusted Cast target playout delay"
                    );
                }
                VideoFeedbackAction::DisableAdaptivePlayoutDelay {
                    requested,
                    receiver,
                } => {
                    tracing::warn!(
                        media_generation = generation.get(),
                        requested_milliseconds = requested.as_millis(),
                        receiver_milliseconds = ?receiver.map(|delay| delay.as_millis()),
                        attempts = MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS,
                        "receiver did not apply adaptive Cast playout delay"
                    );
                }
            }
        }
        Ok(events)
    }

    pub(crate) async fn start(&mut self, media_generation: u64) -> Result<(), MediaSessionError> {
        let generation =
            self.require_generation("Start", media_generation, SessionState::Configured)?;
        if !self.media_ready {
            return Err(MediaSessionError::Graph(
                "Start cannot follow a failed media configuration".into(),
            ));
        }
        let previous_video = self.sender()?.statistics(generation).await?;
        let previous_audio = if self.audio_enabled {
            Some(self.audio_sender()?.statistics(generation).await?)
        } else {
            None
        };
        self.sender()?.start(generation).await?;
        if self.audio_enabled {
            if let Err(error) = self.audio_sender()?.start(generation).await {
                let _ = self.sender()?.suspend(generation).await;
                return Err(error.into());
            }
        }
        let deadline = Instant::now() + START_CONFIRMATION_TIMEOUT;
        if let Err(error) = self.graph.start(generation).await {
            self.suspend_senders_best_effort(generation).await;
            return Err(error.into());
        }
        let confirmation = self
            .wait_for_media_confirmation(
                generation,
                &previous_video,
                previous_audio.as_ref(),
                deadline,
            )
            .await;
        if let Err(error) = confirmation {
            let _ = self.graph.suspend(generation).await;
            self.suspend_senders_best_effort(generation).await;
            return Err(error);
        }
        self.state = SessionState::Streaming;
        Ok(())
    }

    pub(crate) async fn suspend(&mut self) -> Result<(), MediaSessionError> {
        if self.state != SessionState::Streaming {
            return Err(MediaSessionError::Transition {
                operation: "Suspend",
                state: self.state,
            });
        }
        let generation = self
            .active_generation
            .ok_or_else(|| MediaSessionError::Graph("active media generation is missing".into()))?;
        self.graph.suspend(generation).await?;
        if self.audio_enabled {
            if let Err(error) = self.audio_sender()?.suspend(generation).await {
                let _ = self.graph.resume(generation).await;
                return Err(error.into());
            }
        }
        if let Err(error) = self.sender()?.suspend(generation).await {
            if self.audio_enabled {
                let _ = self.audio_sender()?.resume(generation).await;
            }
            let _ = self.graph.resume(generation).await;
            return Err(error.into());
        }
        self.state = SessionState::Suspended;
        Ok(())
    }

    pub(crate) async fn resume(&mut self, media_generation: u64) -> Result<(), MediaSessionError> {
        let generation =
            self.require_generation("Resume", media_generation, SessionState::Suspended)?;
        let previous_video = self.sender()?.statistics(generation).await?;
        let previous_audio = if self.audio_enabled {
            Some(self.audio_sender()?.statistics(generation).await?)
        } else {
            None
        };
        self.sender()?.resume(generation).await?;
        if self.audio_enabled {
            if let Err(error) = self.audio_sender()?.resume(generation).await {
                let _ = self.sender()?.suspend(generation).await;
                return Err(error.into());
            }
        }
        let deadline = Instant::now() + START_CONFIRMATION_TIMEOUT;
        if let Err(error) = self.graph.resume(generation).await {
            self.suspend_senders_best_effort(generation).await;
            return Err(error.into());
        }
        let confirmation = self
            .wait_for_media_confirmation(
                generation,
                &previous_video,
                previous_audio.as_ref(),
                deadline,
            )
            .await;
        if let Err(error) = confirmation {
            let _ = self.graph.suspend(generation).await;
            self.suspend_senders_best_effort(generation).await;
            return Err(error);
        }
        self.state = SessionState::Streaming;
        Ok(())
    }

    pub(crate) async fn stop_media<T: VideoTransportNegotiator + ?Sized>(
        &mut self,
        media_generation: u64,
        transport: &mut T,
    ) -> Result<(), MediaSessionError> {
        let generation = NonZeroU64::new(media_generation).ok_or_else(|| {
            MediaSessionError::InvalidRequest("media generation must be nonzero".into())
        })?;
        if self.state == SessionState::Prepared && self.completed_generation == Some(generation) {
            return Ok(());
        }
        if !matches!(
            self.state,
            SessionState::Configured | SessionState::Streaming | SessionState::Suspended
        ) {
            return Err(MediaSessionError::Transition {
                operation: "StopMedia",
                state: self.state,
            });
        }
        self.require_matching_generation("StopMedia", generation)?;
        let graph_received_generation = self.graph_received_generation;
        let audio_sender_received_generation = self.audio_sender_received_generation;
        let sender_received_generation = self.sender_received_generation;
        let transport_active = self.transport_active;
        let graph = &mut self.graph;
        let audio_sender = self.audio_sender.as_ref();
        let sender = self.sender.as_ref();
        // These owners can all make teardown progress independently. Waiting
        // for one before touching the next would let a wedged graph strand the
        // Cast transport and sender actors until the whole backend is killed.
        let (graph_result, audio_sender_result, sender_result, transport_result) = tokio::join!(
            async {
                if graph_received_generation {
                    graph
                        .stop(generation)
                        .await
                        .map(|_| ())
                        .map_err(MediaSessionError::from)
                } else {
                    Ok(())
                }
            },
            async {
                if audio_sender_received_generation {
                    match audio_sender {
                        Some(sender) => sender
                            .stop(generation)
                            .await
                            .map(|_| ())
                            .map_err(MediaSessionError::from),
                        None => Err(MediaSessionError::Transport(
                            "audio sender actor is shut down".into(),
                        )),
                    }
                } else {
                    Ok(())
                }
            },
            async {
                if sender_received_generation {
                    match sender {
                        Some(sender) => sender
                            .stop(generation)
                            .await
                            .map(|_| ())
                            .map_err(MediaSessionError::from),
                        None => Err(MediaSessionError::Transport(
                            "video sender actor is shut down".into(),
                        )),
                    }
                } else {
                    Ok(())
                }
            },
            async {
                if transport_active {
                    transport
                        .stop_video()
                        .await
                        .map_err(MediaSessionError::from)
                } else {
                    Ok(())
                }
            },
        );
        self.active_generation = None;
        self.completed_generation = Some(generation);
        self.graph_received_generation = false;
        self.sender_received_generation = false;
        self.audio_sender_received_generation = false;
        self.transport_active = false;
        self.audio_enabled = false;
        self.media_ready = false;
        self.video_bitrate = 0;
        self.feedback_controller = None;
        self.state = SessionState::Prepared;
        graph_result
            .and(audio_sender_result)
            .and(sender_result)
            .and(transport_result)
    }

    pub(crate) async fn statistics(&mut self) -> Result<SessionStatistics, MediaSessionError> {
        if !self.media_ready
            || !matches!(
                self.state,
                SessionState::Configured | SessionState::Streaming | SessionState::Suspended
            )
        {
            return Err(MediaSessionError::Transition {
                operation: "GetStatistics",
                state: self.state,
            });
        }
        let generation = self
            .active_generation
            .ok_or_else(|| MediaSessionError::Graph("active media generation is missing".into()))?;
        let graph = self.graph.statistics(generation).await?;
        let sender = self.sender()?.statistics(generation).await?;
        let audio = if self.audio_enabled {
            Some(self.audio_sender()?.statistics(generation).await?)
        } else {
            None
        };
        let queue_delay = audio.as_ref().map_or(sender.queue_delay, |audio| {
            sender.queue_delay.max(audio.queue_delay)
        });
        let queue_delay_micros = u64::try_from(queue_delay.as_micros())
            .unwrap_or(u64::MAX)
            .min(60_000_000);
        let statistics = SessionStatistics {
            session_generation: self.session_generation,
            media_generation: generation.get(),
            video_bitrate: self.video_bitrate,
            // Start/Resume wait for this transport-side count, so success
            // means chromiacast accepted a validated access unit.
            encoded_frames: sender.frames,
            dropped_frames: graph.dropped_frames.saturating_add(sender.dropped_frames),
            queue_delay_micros,
        };
        statistics
            .validate()
            .map_err(|error| MediaSessionError::Graph(error.to_string()))?;
        Ok(statistics)
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), MediaSessionError> {
        let audio_sender = self.audio_sender.take();
        let sender = self.sender.take();
        let (graph_result, audio_sender_result, sender_result) = tokio::join!(
            async { self.graph.shutdown().await.map_err(MediaSessionError::from) },
            async move {
                match audio_sender {
                    Some(sender) => sender.shutdown().await.map_err(MediaSessionError::from),
                    None => Ok(()),
                }
            },
            async move {
                match sender {
                    Some(sender) => sender.shutdown().await.map_err(MediaSessionError::from),
                    None => Ok(()),
                }
            },
        );
        self.active_generation = None;
        self.graph_received_generation = false;
        self.sender_received_generation = false;
        self.audio_sender_received_generation = false;
        self.transport_active = false;
        self.audio_enabled = false;
        self.media_ready = false;
        self.video_bitrate = 0;
        self.feedback_controller = None;
        self.state = SessionState::Stopped;
        graph_result.and(audio_sender_result).and(sender_result)
    }

    fn graph_configuration(
        &self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        generation: NonZeroU64,
    ) -> Result<(MediaGraphConfiguration, VideoTransportConfiguration), MediaSessionError> {
        let capabilities = self
            .capabilities
            .as_ref()
            .ok_or(MediaSessionError::Transition {
                operation: "ConfigureMedia",
                state: self.state,
            })?;
        let audio_profile = match configuration.audio_profile_id.as_deref() {
            Some(profile_id) => {
                if capabilities.features & SESSION_FEATURE_AUDIO == 0 {
                    return Err(MediaSessionError::InvalidRequest(
                        "audio was configured without a negotiated audio capability".into(),
                    ));
                }
                let profile = capabilities
                    .audio_profiles
                    .iter()
                    .find(|profile| profile.profile_id == profile_id)
                    .ok_or_else(|| {
                        MediaSessionError::InvalidRequest(
                            "configured audio profile was not negotiated by Prepare".into(),
                        )
                    })?;
                if profile.codec != "opus"
                    || profile.max_channels < OPUS_CHANNELS as u8
                    || !profile.sample_rates.contains(&OPUS_SAMPLE_RATE)
                {
                    return Err(MediaSessionError::InvalidRequest(
                        "negotiated audio profile cannot carry 48 kHz stereo Opus".into(),
                    ));
                }
                Some(profile)
            }
            None => None,
        };
        if !capabilities.modes.contains(&configuration.mode) {
            return Err(MediaSessionError::InvalidRequest(
                "configured mode was not negotiated by Prepare".into(),
            ));
        }
        let profile = capabilities
            .video_profiles
            .iter()
            .find(|profile| profile.profile_id == configuration.video_profile_id)
            .ok_or_else(|| {
                MediaSessionError::InvalidRequest(
                    "configured video profile was not negotiated by Prepare".into(),
                )
            })?;
        if configuration.mode.width > profile.max_width
            || configuration.mode.height > profile.max_height
            || configuration.mode.refresh_millihz > profile.max_refresh_millihz
        {
            return Err(MediaSessionError::InvalidRequest(
                "configured mode exceeds the negotiated video profile".into(),
            ));
        }

        let mut remotes = remotes.into_iter();
        let video_remote: StdOwnedFd = remotes
            .next()
            .expect("wire validation requires video")
            .into();
        let mut targets = targets.into_iter();
        let video_target = targets.next().expect("wire validation requires video");
        if video_target.kind != MediaKind::Video {
            return Err(MediaSessionError::InvalidRequest(
                "the first PipeWire target is not video".into(),
            ));
        }
        if video_target.session_id != self.session_id {
            return Err(MediaSessionError::InvalidRequest(
                "PipeWire target belongs to another session".into(),
            ));
        }
        let caps = ValidatedVideoCaps::parse(&video_target.caps)?;
        if caps.width.get() != configuration.mode.width
            || caps.height.get() != configuration.mode.height
        {
            return Err(MediaSessionError::InvalidRequest(format!(
                "video caps are {}x{} but configured mode is {}x{}",
                caps.width, caps.height, configuration.mode.width, configuration.mode.height
            )));
        }
        let caps_millihz = u64::from(caps.framerate_numerator.get())
            .checked_mul(1_000)
            .ok_or_else(|| MediaSessionError::InvalidRequest("video refresh overflows".into()))?;
        let configured_millihz = u64::from(configuration.mode.refresh_millihz)
            .checked_mul(u64::from(caps.framerate_denominator.get()))
            .ok_or_else(|| {
                MediaSessionError::InvalidRequest("configured refresh overflows".into())
            })?;
        if caps_millihz != configured_millihz {
            return Err(MediaSessionError::InvalidRequest(
                "video caps refresh differs from the configured mode".into(),
            ));
        }

        let audio = match audio_profile {
            Some(_) => {
                let remote: StdOwnedFd = remotes
                    .next()
                    .expect("wire validation requires audio")
                    .into();
                let target = targets.next().expect("wire validation requires audio");
                if target.kind != MediaKind::Audio {
                    return Err(MediaSessionError::InvalidRequest(
                        "the second PipeWire target is not audio".into(),
                    ));
                }
                if target.session_id != video_target.session_id
                    || target.device_instance != video_target.device_instance
                    || target.connector_id != video_target.connector_id
                    || target.output_index != video_target.output_index
                    || target.media_generation != video_target.media_generation
                {
                    return Err(MediaSessionError::InvalidRequest(
                        "audio target is not paired with the configured video output".into(),
                    ));
                }
                let audio_caps = ValidatedAudioCaps::parse(&target.caps)?;
                Some((
                    PipeWireAudioInput {
                        remote,
                        node_name: target.node_name,
                        object_serial: NonZeroU64::new(target.object_serial)
                            .expect("wire validation rejected zero audio object serial"),
                        caps: target.caps,
                    },
                    AudioTransportConfiguration {
                        sample_rate: audio_caps.sample_rate.get(),
                        channels: u8::try_from(audio_caps.channels.get())
                            .expect("validated audio channel count fits u8"),
                        bitrate: OPUS_BITRATE,
                    },
                ))
            }
            None => None,
        };

        let bitrate = u32::try_from(configuration.video_bitrate).map_err(|_| {
            MediaSessionError::InvalidRequest("video bitrate exceeds Cast's u32 range".into())
        })?;
        let minimum_playout_delay = minimum_playout_delay(
            caps.framerate_numerator.get(),
            caps.framerate_denominator.get(),
            audio.is_some(),
        );
        let transport = VideoTransportConfiguration {
            width: caps.width.get(),
            height: caps.height.get(),
            framerate_numerator: caps.framerate_numerator.get(),
            framerate_denominator: caps.framerate_denominator.get(),
            bitrate,
            target_playout_delay: INITIAL_PLAYOUT_DELAY.max(minimum_playout_delay),
            audio: audio.as_ref().map(|(_, transport)| *transport),
        };
        let graph = MediaGraphConfiguration {
            media_generation: generation,
            video: PipeWireVideoInput {
                remote: video_remote,
                node_name: video_target.node_name,
                object_serial: NonZeroU64::new(video_target.object_serial)
                    .expect("wire validation rejected zero object serial"),
                caps: video_target.caps,
            },
            audio: audio.map(|(input, _)| input),
            video_bitrate: NonZeroU64::new(configuration.video_bitrate)
                .expect("wire validation rejected zero bitrate"),
        };
        Ok((graph, transport))
    }

    async fn wait_for_media_confirmation(
        &mut self,
        generation: NonZeroU64,
        previous_video: &VideoSenderStatistics,
        previous_audio: Option<&AudioSenderStatistics>,
        deadline: Instant,
    ) -> Result<(), MediaSessionError> {
        self.sender()?
            .wait_for_frame_after(
                generation,
                previous_video.frames,
                deadline.saturating_duration_since(Instant::now()),
            )
            .await?;
        if let Some(previous_audio) = previous_audio {
            self.audio_sender()?
                .wait_for_packet_after(
                    generation,
                    previous_audio.packets,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .await?;
        }
        self.sender()?
            .wait_for_receiver_ack_after(
                generation,
                previous_video.acknowledged_frames,
                deadline.saturating_duration_since(Instant::now()),
            )
            .await?;
        if previous_audio.is_some() {
            self.sender()?
                .wait_for_receiver_audio_ack_after(
                    generation,
                    previous_video.acknowledged_audio_packets,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .await?;
        }
        Ok(())
    }

    async fn suspend_senders_best_effort(&mut self, generation: NonZeroU64) {
        if self.audio_enabled {
            if let Ok(sender) = self.audio_sender() {
                let _ = sender.suspend(generation).await;
            }
        }
        if let Ok(sender) = self.sender() {
            let _ = sender.suspend(generation).await;
        }
    }

    fn require_generation(
        &self,
        operation: &'static str,
        media_generation: u64,
        required_state: SessionState,
    ) -> Result<NonZeroU64, MediaSessionError> {
        if self.state != required_state {
            return Err(MediaSessionError::Transition {
                operation,
                state: self.state,
            });
        }
        let generation = NonZeroU64::new(media_generation).ok_or_else(|| {
            MediaSessionError::InvalidRequest("media generation must be nonzero".into())
        })?;
        self.require_matching_generation(operation, generation)?;
        Ok(generation)
    }

    fn require_matching_generation(
        &self,
        operation: &'static str,
        generation: NonZeroU64,
    ) -> Result<(), MediaSessionError> {
        if self.active_generation == Some(generation) {
            Ok(())
        } else {
            Err(MediaSessionError::InvalidRequest(format!(
                "{operation} generation {generation} does not match active generation {:?}",
                self.active_generation
            )))
        }
    }

    fn sender(&self) -> Result<&VideoSenderActor, MediaSessionError> {
        self.sender
            .as_ref()
            .ok_or_else(|| MediaSessionError::Transport("video sender actor is shut down".into()))
    }

    fn audio_sender(&self) -> Result<&AudioSenderActor, MediaSessionError> {
        self.audio_sender
            .as_ref()
            .ok_or_else(|| MediaSessionError::Transport("audio sender actor is shut down".into()))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum MediaSessionError {
    #[error("invalid media request: {0}")]
    InvalidRequest(String),
    #[error("{operation} is invalid while the session is {state:?}")]
    Transition {
        operation: &'static str,
        state: SessionState,
    },
    #[error("Chromiacast media graph failed: {0}")]
    Graph(String),
    #[error("Chromiacast video transport failed: {0}")]
    Transport(String),
}

impl From<MediaGraphError> for MediaSessionError {
    fn from(error: MediaGraphError) -> Self {
        Self::Graph(error.to_string())
    }
}

impl From<VideoTransportError> for MediaSessionError {
    fn from(error: VideoTransportError) -> Self {
        Self::Transport(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use pronk_backend_protocol::{
        AudioProfile, DisplayIdentity, DisplayMode, IdentitySource, VideoProfile,
        SESSION_FEATURE_AUDIO,
    };
    use pronk_media::VideoFrameDependency;

    use super::*;
    use crate::transport::{
        AudioSendOutcome, AudioSenderPort, NegotiatedVideoTransport, VideoSendOutcome,
        VideoSenderPort, VideoTransportError, VideoTransportFeedbackSnapshot,
    };

    #[derive(Debug)]
    struct FakeGraph {
        generation: Option<NonZeroU64>,
        statistics: MediaGraphStatistics,
        output: mpsc::Sender<EncodedVideoAccessUnit>,
        audio_output: Option<mpsc::Sender<EncodedAudioPacket>>,
        audio_enabled: bool,
        block_stop: bool,
    }

    impl FakeGraph {
        fn video(output: mpsc::Sender<EncodedVideoAccessUnit>) -> Self {
            Self {
                generation: None,
                statistics: MediaGraphStatistics::default(),
                output,
                audio_output: None,
                audio_enabled: false,
                block_stop: false,
            }
        }

        fn audio(
            output: mpsc::Sender<EncodedVideoAccessUnit>,
            audio_output: mpsc::Sender<EncodedAudioPacket>,
        ) -> Self {
            Self {
                audio_output: Some(audio_output),
                ..Self::video(output)
            }
        }
    }

    #[async_trait]
    impl MediaGraphPort for FakeGraph {
        async fn configure(
            &mut self,
            configuration: MediaGraphConfiguration,
        ) -> Result<(), MediaGraphError> {
            self.generation = Some(configuration.media_generation);
            self.statistics.video_bitrate = configuration.video_bitrate.get();
            drop(configuration.video.remote);
            self.audio_enabled = configuration.audio.is_some();
            if let Some(audio) = configuration.audio {
                drop(audio.remote);
            }
            Ok(())
        }

        async fn start(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
            assert_eq!(self.generation, Some(generation));
            self.statistics.frames = 1;
            self.statistics.first_pts_nanos = Some(10);
            self.statistics.last_pts_nanos = Some(10);
            self.output
                .send(access_unit(generation, 10))
                .await
                .map_err(|_| MediaGraphError::new("fake encoded output closed"))?;
            if self.audio_enabled {
                self.statistics.audio_packets = 1;
                self.statistics.first_audio_pts_nanos = Some(10);
                self.statistics.last_audio_pts_nanos = Some(10);
                self.audio_output
                    .as_ref()
                    .expect("audio graph has output")
                    .send(audio_packet(generation, 10))
                    .await
                    .map_err(|_| MediaGraphError::new("fake encoded-audio output closed"))?;
            }
            Ok(())
        }

        async fn suspend(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
            assert_eq!(self.generation, Some(generation));
            Ok(())
        }

        async fn resume(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
            assert_eq!(self.generation, Some(generation));
            self.statistics.frames += 1;
            self.statistics.last_pts_nanos = Some(20);
            self.output
                .send(access_unit(generation, 20))
                .await
                .map_err(|_| MediaGraphError::new("fake encoded output closed"))?;
            if self.audio_enabled {
                self.statistics.audio_packets += 1;
                self.statistics.last_audio_pts_nanos = Some(20);
                self.audio_output
                    .as_ref()
                    .expect("audio graph has output")
                    .send(audio_packet(generation, 20))
                    .await
                    .map_err(|_| MediaGraphError::new("fake encoded-audio output closed"))?;
            }
            Ok(())
        }

        async fn request_key_frame(
            &mut self,
            generation: NonZeroU64,
        ) -> Result<(), MediaGraphError> {
            assert_eq!(self.generation, Some(generation));
            self.statistics.key_frame_requests =
                self.statistics.key_frame_requests.saturating_add(1);
            Ok(())
        }

        async fn set_video_bitrate(
            &mut self,
            generation: NonZeroU64,
            bitrate: NonZeroU64,
        ) -> Result<u64, MediaGraphError> {
            assert_eq!(self.generation, Some(generation));
            self.statistics.video_bitrate = bitrate.get();
            self.statistics.bitrate_changes = self.statistics.bitrate_changes.saturating_add(1);
            Ok(bitrate.get())
        }

        async fn stop(
            &mut self,
            generation: NonZeroU64,
        ) -> Result<MediaGraphStatistics, MediaGraphError> {
            assert_eq!(self.generation.take(), Some(generation));
            if self.block_stop {
                std::future::pending::<()>().await;
            }
            Ok(self.statistics.clone())
        }

        async fn statistics(
            &mut self,
            generation: NonZeroU64,
        ) -> Result<MediaGraphStatistics, MediaGraphError> {
            assert_eq!(self.generation, Some(generation));
            Ok(self.statistics.clone())
        }

        async fn shutdown(&mut self) -> Result<(), MediaGraphError> {
            self.generation = None;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeSender {
        feedback: watch::Sender<VideoTransportFeedbackSnapshot>,
        playout_delays: Option<Arc<Mutex<Vec<Duration>>>>,
    }

    #[derive(Debug)]
    struct FakeAudioSender {
        feedback: watch::Sender<VideoTransportFeedbackSnapshot>,
    }

    #[async_trait]
    impl AudioSenderPort for FakeAudioSender {
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
    impl VideoSenderPort for FakeSender {
        fn supports_target_playout_delay_updates(&self) -> bool {
            self.playout_delays.is_some()
        }

        async fn set_target_playout_delay(
            &mut self,
            delay: Duration,
        ) -> Result<(), VideoTransportError> {
            self.playout_delays
                .as_ref()
                .ok_or_else(|| VideoTransportError::new("adaptive playout delay is disabled"))?
                .lock()
                .unwrap()
                .push(delay);
            Ok(())
        }

        async fn send(
            &mut self,
            _access_unit: EncodedVideoAccessUnit,
        ) -> Result<VideoSendOutcome, VideoTransportError> {
            self.feedback.send_modify(|snapshot| {
                snapshot.revision = snapshot.revision.saturating_add(1);
                snapshot.acknowledged_frames = snapshot.acknowledged_frames.saturating_add(1);
            });
            Ok(VideoSendOutcome::Accepted)
        }

        async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct FakeTransport {
        configuration: Option<VideoTransportConfiguration>,
        stops: u32,
    }

    #[derive(Debug)]
    struct AdaptiveTransport {
        playout_delays: Arc<Mutex<Vec<Duration>>>,
    }

    #[async_trait]
    impl VideoTransportNegotiator for AdaptiveTransport {
        async fn negotiate_video(
            &mut self,
            _configuration: VideoTransportConfiguration,
        ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
            let (feedback, receiver) = watch::channel(VideoTransportFeedbackSnapshot::default());
            Ok(NegotiatedVideoTransport {
                sender: Box::new(FakeSender {
                    feedback,
                    playout_delays: Some(self.playout_delays.clone()),
                }),
                audio_sender: None,
                feedback: receiver,
                minimum_bitrate: None,
            })
        }

        async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    #[async_trait]
    impl VideoTransportNegotiator for FakeTransport {
        async fn negotiate_video(
            &mut self,
            configuration: VideoTransportConfiguration,
        ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
            let with_audio = configuration.audio.is_some();
            self.configuration = Some(configuration);
            Ok(fake_transport(with_audio))
        }

        async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
            self.stops += 1;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FailingTransport;

    #[async_trait]
    impl VideoTransportNegotiator for FailingTransport {
        async fn negotiate_video(
            &mut self,
            _configuration: VideoTransportConfiguration,
        ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
            Err(VideoTransportError::new("scripted negotiation failure"))
        }

        async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
            Ok(())
        }
    }

    fn fake_transport(with_audio: bool) -> NegotiatedVideoTransport {
        let (feedback, receiver) = watch::channel(VideoTransportFeedbackSnapshot::default());
        NegotiatedVideoTransport {
            sender: Box::new(FakeSender {
                feedback: feedback.clone(),
                playout_delays: None,
            }),
            audio_sender: with_audio
                .then(|| Box::new(FakeAudioSender { feedback }) as Box<dyn AudioSenderPort>),
            feedback: receiver,
            minimum_bitrate: None,
        }
    }

    #[test]
    fn playout_delay_floor_covers_one_video_or_audio_packet() {
        assert_eq!(
            minimum_playout_delay(60, 1, false),
            Duration::from_millis(17)
        );
        assert_eq!(minimum_playout_delay(60, 1, true), OPUS_FRAME_DURATION);
        assert_eq!(
            minimum_playout_delay(30, 1, false),
            Duration::from_millis(34)
        );
        assert_eq!(
            minimum_playout_delay(60_000, 1_001, false),
            Duration::from_millis(17)
        );
    }

    #[tokio::test]
    async fn negotiated_session_owns_generation_transitions_and_exact_target() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();

        let mut wrong_target = target(session_id, 1);
        wrong_target.session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into();
        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![wrong_target],
                    configuration(),
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::InvalidRequest(_))
        ));

        media
            .configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();
        assert_eq!(
            transport.configuration,
            Some(VideoTransportConfiguration {
                width: 640,
                height: 480,
                framerate_numerator: 60,
                framerate_denominator: 1,
                bitrate: 2_000_000,
                target_playout_delay: INITIAL_PLAYOUT_DELAY,
                audio: None,
            })
        );
        assert!(matches!(
            media.start(2).await,
            Err(MediaSessionError::InvalidRequest(_))
        ));
        media.start(1).await.unwrap();
        let first = media.statistics().await.unwrap();
        assert_eq!(first.session_generation, 7);
        assert_eq!(first.media_generation, 1);
        assert_eq!(first.encoded_frames, 1);
        media.suspend().await.unwrap();
        media.resume(1).await.unwrap();
        assert_eq!(media.statistics().await.unwrap().encoded_frames, 2);
        media.stop_media(1, &mut transport).await.unwrap();
        media.stop_media(1, &mut transport).await.unwrap();
        assert_eq!(transport.stops, 1);
        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![target(session_id, 1)],
                    configuration(),
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::InvalidRequest(_))
        ));
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn audio_generation_requires_exact_pairing_and_receiver_acknowledgement() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (video_output, video_receiver) = mpsc::channel(4);
        let (audio_output, audio_receiver) = mpsc::channel(8);
        let graph = FakeGraph::audio(video_output, audio_output);
        let mut media = ChromiacastMediaSession::with_graph_outputs(
            session_id.into(),
            7,
            Box::new(graph),
            video_receiver,
            audio_receiver,
        );
        let mut transport = FakeTransport::default();
        media.complete_preparation(audio_capabilities()).unwrap();

        let mut wrong_audio = audio_target(session_id, 1);
        wrong_audio.output_index = 1;
        assert!(matches!(
            media
                .configure(
                    audio_remotes(),
                    vec![target(session_id, 1), wrong_audio],
                    audio_configuration(),
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::InvalidRequest(_))
        ));

        media
            .configure(
                audio_remotes(),
                vec![target(session_id, 1), audio_target(session_id, 1)],
                audio_configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();
        assert_eq!(
            transport.configuration,
            Some(VideoTransportConfiguration {
                width: 640,
                height: 480,
                framerate_numerator: 60,
                framerate_denominator: 1,
                bitrate: 2_000_000,
                target_playout_delay: INITIAL_PLAYOUT_DELAY,
                audio: Some(AudioTransportConfiguration {
                    sample_rate: OPUS_SAMPLE_RATE,
                    channels: 2,
                    bitrate: OPUS_BITRATE,
                }),
            })
        );
        media.start(1).await.unwrap();
        assert_eq!(
            media
                .audio_sender()
                .unwrap()
                .statistics(NonZeroU64::new(1).unwrap())
                .await
                .unwrap()
                .packets,
            1
        );
        assert_eq!(
            media
                .sender()
                .unwrap()
                .statistics(NonZeroU64::new(1).unwrap())
                .await
                .unwrap()
                .acknowledged_audio_packets,
            1
        );
        media.suspend().await.unwrap();
        media.resume(1).await.unwrap();
        assert_eq!(
            media
                .audio_sender()
                .unwrap()
                .statistics(NonZeroU64::new(1).unwrap())
                .await
                .unwrap()
                .packets,
            2
        );
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn matching_stop_cleans_graph_after_transport_negotiation_failure() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FailingTransport;
        media.complete_preparation(capabilities()).unwrap();

        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![target(session_id, 1)],
                    configuration(),
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::Transport(_))
        ));
        media.stop_media(1, &mut transport).await.unwrap();
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wedged_graph_stop_does_not_skip_transport_teardown() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let mut graph = FakeGraph::video(output);
        graph.block_stop = true;
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        media
            .configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();
        media.start(1).await.unwrap();

        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            media.stop_media(1, &mut transport),
        )
        .await
        .is_err());
        assert_eq!(transport.stops, 1);
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generation_scoped_feedback_mutates_only_the_media_graph() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        media
            .configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();

        assert!(media
            .handle_feedback(VideoSenderFeedbackSnapshot {
                revision: 1,
                generation: NonZeroU64::new(2),
                key_frame_requests: 1,
                pressure: Some(overloaded_pressure()),
                ..VideoSenderFeedbackSnapshot::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert_eq!(media.statistics().await.unwrap().video_bitrate, 2_000_000);

        assert_eq!(
            media
                .handle_feedback(VideoSenderFeedbackSnapshot {
                    revision: 2,
                    generation: NonZeroU64::new(1),
                    key_frame_requests: 1,
                    pressure: Some(overloaded_pressure()),
                    ..VideoSenderFeedbackSnapshot::default()
                })
                .await
                .unwrap(),
            [
                MediaSessionEvent::KeyFrameRequested {
                    session_generation: 7,
                    media_generation: 1,
                },
                MediaSessionEvent::BitrateRequested {
                    session_generation: 7,
                    media_generation: 1,
                    bitrate: 1_600_000,
                },
            ]
        );
        assert_eq!(media.statistics().await.unwrap().video_bitrate, 1_600_000);
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn adaptive_feedback_updates_the_generation_owned_transport() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let playout_delays = Arc::new(Mutex::new(Vec::new()));
        let mut transport = AdaptiveTransport {
            playout_delays: playout_delays.clone(),
        };
        media.complete_preparation(capabilities()).unwrap();
        media
            .configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();

        assert!(media
            .handle_feedback(VideoSenderFeedbackSnapshot {
                revision: 1,
                generation: NonZeroU64::new(1),
                pressure: Some(crate::transport::VideoTransportPressure {
                    receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
                    nack_count: 1,
                    ..crate::transport::VideoTransportPressure::default()
                }),
                ..VideoSenderFeedbackSnapshot::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert_eq!(*playout_delays.lock().unwrap(), [Duration::from_millis(66)]);

        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn terminal_feedback_is_fatal_only_for_the_active_generation() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        media
            .configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();

        let terminal = VideoTransportError::new("scripted terminal sender failure");
        assert!(media
            .handle_feedback(VideoSenderFeedbackSnapshot {
                generation: NonZeroU64::new(2),
                terminal_error: Some(terminal.clone()),
                ..VideoSenderFeedbackSnapshot::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            media
                .handle_feedback(VideoSenderFeedbackSnapshot {
                    generation: NonZeroU64::new(1),
                    terminal_error: Some(terminal),
                    ..VideoSenderFeedbackSnapshot::default()
                })
                .await,
            Err(MediaSessionError::Transport(
                "scripted terminal sender failure".into()
            ))
        );
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    fn overloaded_pressure() -> crate::transport::VideoTransportPressure {
        crate::transport::VideoTransportPressure {
            in_flight_frames: 12,
            in_flight_media_duration: Duration::from_millis(250),
            max_acceptable_in_flight_duration: Duration::from_millis(100),
            ..crate::transport::VideoTransportPressure::default()
        }
    }

    fn capabilities() -> DeviceCapabilities {
        DeviceCapabilities {
            preparation_generation: 1,
            display_identity: DisplayIdentity {
                manufacturer_name: Some("Sony".into()),
                manufacturer_source: IdentitySource::SetupEndpoint,
                product_name: Some("BRAVIA".into()),
                product_source: IdentitySource::SetupEndpoint,
                pnp_id: None,
            },
            modes: vec![mode()],
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 640,
                max_height: 480,
                max_refresh_millihz: 60_000,
            }],
            audio_profiles: Vec::new(),
            features: 0,
        }
    }

    fn audio_capabilities() -> DeviceCapabilities {
        let mut capabilities = capabilities();
        capabilities.features = SESSION_FEATURE_AUDIO;
        capabilities.audio_profiles = vec![AudioProfile {
            profile_id: "opus-stereo".into(),
            codec: "opus".into(),
            max_channels: 2,
            sample_rates: vec![OPUS_SAMPLE_RATE],
        }];
        capabilities
    }

    fn configuration() -> MediaConfiguration {
        MediaConfiguration {
            video_profile_id: "h264-high".into(),
            audio_profile_id: None,
            mode: mode(),
            video_bitrate: 2_000_000,
        }
    }

    fn audio_configuration() -> MediaConfiguration {
        MediaConfiguration {
            audio_profile_id: Some("opus-stereo".into()),
            ..configuration()
        }
    }

    fn mode() -> DisplayMode {
        DisplayMode {
            width: 640,
            height: 480,
            refresh_millihz: 60_000,
            flags: 0,
        }
    }

    fn target(session_id: &str, media_generation: u64) -> PipeWireTarget {
        PipeWireTarget {
            kind: MediaKind::Video,
            node_name: "pronk.test.video".into(),
            object_serial: 42,
            session_id: session_id.into(),
            device_instance: "test-card".into(),
            connector_id: 40,
            output_index: 0,
            media_generation,
            caps: "video/x-raw,format=BGRx,width=640,height=480,framerate=60/1".into(),
        }
    }

    fn audio_target(session_id: &str, media_generation: u64) -> PipeWireTarget {
        PipeWireTarget {
            kind: MediaKind::Audio,
            node_name: "alsa_output.castkms.stereo-fallback".into(),
            object_serial: 43,
            caps: "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2".into(),
            ..target(session_id, media_generation)
        }
    }

    fn remote() -> Vec<OwnedFd> {
        let (remote, peer) = UnixStream::pair().unwrap();
        drop(peer);
        vec![StdOwnedFd::from(remote).into()]
    }

    fn audio_remotes() -> Vec<OwnedFd> {
        let mut remotes = remote();
        remotes.extend(remote());
        remotes
    }

    fn access_unit(generation: NonZeroU64, timestamp: u64) -> EncodedVideoAccessUnit {
        EncodedVideoAccessUnit {
            media_generation: generation,
            dependency: VideoFrameDependency::KeyFrame,
            data: vec![0, 0, 0, 1, 0x65],
            media_timestamp: Duration::from_nanos(timestamp),
            reference_time: Instant::now(),
            duration: Duration::from_millis(16),
        }
    }

    fn audio_packet(generation: NonZeroU64, timestamp: u64) -> EncodedAudioPacket {
        EncodedAudioPacket {
            media_generation: generation,
            data: vec![0xf8, 0xff, 0xfe],
            media_timestamp: Duration::from_nanos(timestamp),
            reference_time: Instant::now(),
            duration: Duration::from_millis(20),
        }
    }
}
