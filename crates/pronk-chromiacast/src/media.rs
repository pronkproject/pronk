mod configuration;
mod encoder_policy;
mod graph;

use std::num::{NonZeroU32, NonZeroU64};
use std::time::{Duration, Instant};

use pronk_backend_protocol::{
    validate_media_configuration, DeviceCapabilities, DisplayMode, MediaConfiguration,
    PipeWireTarget, RawVideoLayout, SessionState, SessionStatistics, Validate,
};
use pronk_media::{
    EncodedAudioPacket, EncodedVideoAccessUnit, MediaGraphError, MediaGraphStatistics,
    VideoCadence, VideoCodec, OPUS_FRAME_DURATION,
};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use zbus::zvariant::OwnedFd;

use crate::audio_sender_actor::{AudioSenderActor, AudioSenderStatistics};
use crate::feedback::{
    AdaptivePlayoutDelayConfiguration, VideoFeedbackAction, VideoFeedbackController,
    MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS,
};
use crate::sender_actor::{VideoSenderActor, VideoSenderFeedbackSnapshot, VideoSenderStatistics};
use crate::transport::{NegotiatedVideoTransport, VideoTransportError, VideoTransportNegotiator};
use configuration::graph_configuration;
pub(crate) use encoder_policy::VideoEncoderPolicy;
use graph::{GStreamerMediaGraph, MediaGraphPort};

const START_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn chromecast_video_cadence() -> VideoCadence {
    // Receiver acknowledgements and advertised rates do not establish visible
    // playback above 30 fps. Keep the encoded cadence independent of KMS refresh.
    VideoCadence::new(
        NonZeroU32::new(30).expect("Chromecast video cadence is nonzero"),
        NonZeroU32::new(1).expect("Chromecast video cadence denominator is nonzero"),
    )
}

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

fn total_dropped_frames(graph: &MediaGraphStatistics, sender: &VideoSenderStatistics) -> u64 {
    graph
        .dropped_video_frames()
        .saturating_add(sender.dropped_frames)
}

/// Session-local media state machine. It owns generation admission and the
/// backend media graph but has no D-Bus, discovery, or Cast control concerns.
#[derive(Debug)]
pub(crate) struct ChromiacastMediaSession {
    session_id: String,
    session_generation: u64,
    state: SessionState,
    capabilities: Option<DeviceCapabilities>,
    generation: GenerationSlot,
    encoder_policy: VideoEncoderPolicy,
    graph: Box<dyn MediaGraphPort>,
    sender: Option<VideoSenderActor>,
    audio_sender: Option<AudioSenderActor>,
}

#[derive(Debug)]
enum GenerationSlot {
    Unused,
    Active(Box<ActiveGeneration>),
    Completed(NonZeroU64),
}

#[derive(Debug)]
struct ActiveGeneration {
    id: NonZeroU64,
    configuration_stage: ConfigurationStage,
    audio_enabled: bool,
    video_bitrate: u64,
    feedback_controller: Option<VideoFeedbackController>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigurationStage {
    NegotiatingTransport,
    ConfiguringGraph,
    ConfiguringAudioSender,
    ConfiguringVideoSender,
    Complete,
}

impl ActiveGeneration {
    fn new(id: NonZeroU64, audio_enabled: bool, video_bitrate: NonZeroU64) -> Self {
        Self {
            id,
            configuration_stage: ConfigurationStage::NegotiatingTransport,
            audio_enabled,
            video_bitrate: video_bitrate.get(),
            feedback_controller: None,
        }
    }

    fn is_ready(&self) -> bool {
        self.configuration_stage == ConfigurationStage::Complete
    }

    fn graph_may_own_generation(&self) -> bool {
        self.configuration_stage != ConfigurationStage::NegotiatingTransport
    }

    fn audio_sender_may_own_generation(&self) -> bool {
        self.audio_enabled
            && matches!(
                self.configuration_stage,
                ConfigurationStage::ConfiguringAudioSender
                    | ConfigurationStage::ConfiguringVideoSender
                    | ConfigurationStage::Complete
            )
    }

    fn video_sender_may_own_generation(&self) -> bool {
        matches!(
            self.configuration_stage,
            ConfigurationStage::ConfiguringVideoSender | ConfigurationStage::Complete
        )
    }
}

impl GenerationSlot {
    fn active(&self) -> Option<NonZeroU64> {
        match self {
            Self::Active(generation) => Some(generation.id),
            Self::Unused | Self::Completed(_) => None,
        }
    }

    fn active_generation(&self) -> Option<&ActiveGeneration> {
        match self {
            Self::Active(generation) => Some(generation.as_ref()),
            Self::Unused | Self::Completed(_) => None,
        }
    }

    fn active_generation_mut(&mut self) -> Option<&mut ActiveGeneration> {
        match self {
            Self::Active(generation) => Some(generation.as_mut()),
            Self::Unused | Self::Completed(_) => None,
        }
    }

    fn completed(&self) -> Option<NonZeroU64> {
        match self {
            Self::Completed(generation) => Some(*generation),
            Self::Unused | Self::Active(_) => None,
        }
    }
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
    fn active_generation(&self) -> &ActiveGeneration {
        self.generation
            .active_generation()
            .expect("configured media session owns an active generation")
    }

    fn active_generation_mut(&mut self) -> &mut ActiveGeneration {
        self.generation
            .active_generation_mut()
            .expect("configured media session owns an active generation")
    }

    pub(crate) fn raw_video_layouts(&self) -> &[RawVideoLayout] {
        self.encoder_policy.raw_layouts()
    }

    pub(crate) fn supported_video_layouts(
        &self,
        modes: &[DisplayMode],
    ) -> Result<Vec<Vec<RawVideoLayout>>, MediaSessionError> {
        let VideoEncoderPolicy::VaH264 { .. } = &self.encoder_policy else {
            return Ok(vec![
                self.encoder_policy.raw_layouts().to_vec();
                modes.len()
            ]);
        };
        let dimensions = modes
            .iter()
            .map(|mode| (mode.width, mode.height))
            .collect::<Vec<_>>();
        let formats = self
            .encoder_policy
            .encoder(VideoCodec::H264)?
            .supported_dma_buf_formats_for_dimensions(&dimensions, chromecast_video_cadence())?;
        Ok(formats
            .into_iter()
            .map(|formats| {
                formats
                    .into_iter()
                    .map(|format| RawVideoLayout::dma_buf(format.format, format.modifier))
                    .filter(|layout| self.encoder_policy.raw_layouts().contains(layout))
                    .collect()
            })
            .collect())
    }

    pub(crate) fn spawn(
        session_id: String,
        session_generation: u64,
        encoder_policy: VideoEncoderPolicy,
    ) -> Result<Self, MediaSessionError> {
        let (graph, outputs) = GStreamerMediaGraph::spawn()?;
        Ok(Self::with_graph_outputs(
            session_id,
            session_generation,
            encoder_policy,
            Box::new(graph),
            outputs.video,
            outputs.audio,
        ))
    }

    fn with_graph_outputs(
        session_id: String,
        session_generation: u64,
        encoder_policy: VideoEncoderPolicy,
        graph: Box<dyn MediaGraphPort>,
        video_output: mpsc::Receiver<EncodedVideoAccessUnit>,
        audio_output: mpsc::Receiver<EncodedAudioPacket>,
    ) -> Self {
        Self {
            session_id,
            session_generation,
            state: SessionState::Created,
            capabilities: None,
            generation: GenerationSlot::Unused,
            encoder_policy,
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
            VideoEncoderPolicy::Software,
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
            .generation
            .completed()
            .is_some_and(|completed| generation <= completed)
        {
            return Err(MediaSessionError::InvalidRequest(format!(
                "media generation {generation} is not newer than completed generation {:?}",
                self.generation.completed()
            )));
        }
        let (graph_configuration, transport_configuration) = graph_configuration(
            &self.session_id,
            self.capabilities
                .as_ref()
                .ok_or(MediaSessionError::Transition {
                    operation: "ConfigureMedia",
                    state: self.state,
                })?,
            &self.encoder_policy,
            remotes,
            targets,
            configuration,
            generation,
        )?;
        let audio_enabled = graph_configuration.audio.is_some();
        let video_bitrate = graph_configuration.video_bitrate;

        // Admit ownership before crossing an asynchronous service boundary.
        // Once the method has consumed its passed fd, matching StopMedia must
        // remain valid even if negotiation or graph setup fails, or the D-Bus
        // reply is lost.
        self.generation = GenerationSlot::Active(Box::new(ActiveGeneration::new(
            generation,
            audio_enabled,
            video_bitrate,
        )));
        self.state = SessionState::Configured;

        // Negotiation can launch the receiver app before its reply reaches us.
        let mut negotiated = transport.negotiate_video(transport_configuration).await?;
        let graph_configuration =
            match graph_configuration.with_encoder(&self.encoder_policy, negotiated.video_codec) {
                Ok(configuration) => configuration,
                Err(error) => {
                    discard_negotiated_transport(negotiated).await;
                    return Err(error.into());
                }
            };
        self.active_generation_mut().configuration_stage = ConfigurationStage::ConfiguringGraph;
        if let Err(error) = self.graph.configure(graph_configuration).await {
            discard_negotiated_transport(negotiated).await;
            return Err(error.into());
        }
        match self.graph.statistics(generation).await {
            Ok(graph_path) => tracing::info!(
                media_generation = generation.get(),
                video_encoder = graph_path.encoder_name.as_deref().unwrap_or("unknown"),
                video_memory_path = graph_path.video_memory_path.as_deref().unwrap_or("unknown"),
                render_device = graph_path.render_device.as_deref().unwrap_or("none"),
                "configured video encoding path"
            ),
            Err(error) => tracing::warn!(
                media_generation = generation.get(),
                %error,
                "could not inspect configured video encoding path"
            ),
        }
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
        let encoder_minimum_bitrate = self.encoder_policy.minimum_bitrate();
        self.active_generation_mut().feedback_controller = Some(VideoFeedbackController::new(
            video_bitrate,
            negotiated.minimum_bitrate,
            encoder_minimum_bitrate,
            adaptive_playout_delay,
        ));

        match (audio_enabled, negotiated.audio_sender.take()) {
            (true, Some(audio_sender)) => {
                // The actor can accept the sender before this request returns.
                self.active_generation_mut().configuration_stage =
                    ConfigurationStage::ConfiguringAudioSender;
                if let Err(error) = self
                    .audio_sender()?
                    .configure(generation, audio_sender)
                    .await
                {
                    let _ = negotiated.sender.shutdown().await;
                    return Err(error.into());
                }
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
        self.active_generation_mut().configuration_stage =
            ConfigurationStage::ConfiguringVideoSender;
        self.sender()?.configure(generation, negotiated).await?;
        self.active_generation_mut().configuration_stage = ConfigurationStage::Complete;
        Ok(())
    }

    pub(crate) async fn handle_feedback(
        &mut self,
        feedback: VideoSenderFeedbackSnapshot,
    ) -> Result<Vec<MediaSessionEvent>, MediaSessionError> {
        let Some(generation) = feedback.generation else {
            return Ok(Vec::new());
        };
        if self.generation.active() != Some(generation) || !self.active_generation().is_ready() {
            return Ok(Vec::new());
        }
        if let Some(error) = feedback.terminal_error {
            return Err(error.into());
        }
        let Some(controller) = self.active_generation_mut().feedback_controller.as_mut() else {
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
                    self.active_generation_mut().video_bitrate = bitrate;
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
        if !self.active_generation().is_ready() {
            return Err(MediaSessionError::Graph(
                "Start cannot follow a failed media configuration".into(),
            ));
        }
        let previous_video = self.sender()?.statistics(generation).await?;
        let previous_audio = if self.active_generation().audio_enabled {
            Some(self.audio_sender()?.statistics(generation).await?)
        } else {
            None
        };
        self.sender()?.start(generation).await?;
        if self.active_generation().audio_enabled {
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
            .generation
            .active()
            .ok_or_else(|| MediaSessionError::Graph("active media generation is missing".into()))?;
        self.graph.suspend(generation).await?;
        if self.active_generation().audio_enabled {
            if let Err(error) = self.audio_sender()?.suspend(generation).await {
                let _ = self.graph.resume(generation).await;
                return Err(error.into());
            }
        }
        if let Err(error) = self.sender()?.suspend(generation).await {
            if self.active_generation().audio_enabled {
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
        let previous_audio = if self.active_generation().audio_enabled {
            Some(self.audio_sender()?.statistics(generation).await?)
        } else {
            None
        };
        self.sender()?.resume(generation).await?;
        if self.active_generation().audio_enabled {
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
        self.stop_media_inner(media_generation, Some(transport))
            .await
    }

    /// Stop local media owners while the containing Device session is leaving.
    ///
    /// The following Device shutdown drops the control connection, which
    /// closes the receiver-side application. Do not make display removal wait
    /// for a receiver Stop request or its protocol timeout.
    pub(crate) async fn abort_media(
        &mut self,
        media_generation: u64,
    ) -> Result<(), MediaSessionError> {
        self.stop_media_inner::<dyn VideoTransportNegotiator>(media_generation, None)
            .await
    }

    async fn stop_media_inner<T: VideoTransportNegotiator + ?Sized>(
        &mut self,
        media_generation: u64,
        transport: Option<&mut T>,
    ) -> Result<(), MediaSessionError> {
        let generation = NonZeroU64::new(media_generation).ok_or_else(|| {
            MediaSessionError::InvalidRequest("media generation must be nonzero".into())
        })?;
        if self.state == SessionState::Prepared && self.generation.completed() == Some(generation) {
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
        let active = self.active_generation();
        let graph_may_own_generation = active.graph_may_own_generation();
        let audio_sender_may_own_generation = active.audio_sender_may_own_generation();
        let sender_may_own_generation = active.video_sender_may_own_generation();
        let graph = &mut self.graph;
        let audio_sender = self.audio_sender.as_ref();
        let sender = self.sender.as_ref();
        // These owners can all make teardown progress independently. Waiting
        // for one before touching the next would let a wedged graph strand the
        // Cast transport and sender actors until the whole backend is killed.
        let (graph_result, audio_sender_result, sender_result, transport_result) = tokio::join!(
            async {
                if graph_may_own_generation {
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
                if audio_sender_may_own_generation {
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
                if sender_may_own_generation {
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
                match transport {
                    Some(transport) => transport
                        .stop_video()
                        .await
                        .map_err(MediaSessionError::from),
                    None => Ok(()),
                }
            },
        );
        self.generation = GenerationSlot::Completed(generation);
        self.state = SessionState::Prepared;
        graph_result
            .and(audio_sender_result)
            .and(sender_result)
            .and(transport_result)
    }

    pub(crate) async fn statistics(&mut self) -> Result<SessionStatistics, MediaSessionError> {
        if !self
            .generation
            .active_generation()
            .is_some_and(ActiveGeneration::is_ready)
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
            .generation
            .active()
            .ok_or_else(|| MediaSessionError::Graph("active media generation is missing".into()))?;
        let graph = self.graph.statistics(generation).await?;
        let sender = self.sender()?.statistics(generation).await?;
        let audio = if self.active_generation().audio_enabled {
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
            video_bitrate: self.active_generation().video_bitrate,
            // Start/Resume wait for this transport-side count, so success
            // means chromiacast accepted a validated access unit.
            encoded_frames: sender.frames,
            dropped_frames: total_dropped_frames(&graph, &sender),
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
        self.generation = GenerationSlot::Unused;
        self.state = SessionState::Stopped;
        graph_result.and(audio_sender_result).and(sender_result)
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
        if self.active_generation().audio_enabled {
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
        if self.generation.active() == Some(generation) {
            Ok(())
        } else {
            Err(MediaSessionError::InvalidRequest(format!(
                "{operation} generation {generation} does not match active generation {:?}",
                self.generation.active()
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

async fn discard_negotiated_transport(negotiated: NegotiatedVideoTransport) {
    let video = negotiated.sender.shutdown();
    let audio = async move {
        match negotiated.audio_sender {
            Some(sender) => sender.shutdown().await,
            None => Ok(()),
        }
    };
    let _ = tokio::join!(video, audio);
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
    use async_trait::async_trait;
    use std::os::fd::OwnedFd as StdOwnedFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use pronk_backend_protocol::{
        AudioProfile, DisplayIdentity, DisplayMode, IdentitySource, MediaKind,
        RenderDeviceIdentity, VideoProfile, SESSION_FEATURE_AUDIO,
    };
    use pronk_media::{
        MediaGraphConfiguration, VideoFrameDependency, OPUS_BITRATE, OPUS_SAMPLE_RATE,
    };

    use super::*;
    use crate::feedback::INITIAL_PLAYOUT_DELAY;
    use crate::transport::{
        AudioSendOutcome, AudioSenderPort, AudioTransportConfiguration, NegotiatedVideoTransport,
        VideoOffer, VideoSendOutcome, VideoSenderPort, VideoTransportConfiguration,
        VideoTransportError, VideoTransportFeedbackSnapshot,
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
            self.statistics.video_cadence_numerator = configuration.video_cadence.numerator.get();
            self.statistics.video_cadence_denominator =
                configuration.video_cadence.denominator.get();
            self.statistics.encoder_name = Some(
                match configuration.video_encoder.codec() {
                    VideoCodec::Vp8 => "vp8enc",
                    VideoCodec::H264 => "x264enc",
                }
                .into(),
            );
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
        shutdowns: Option<Arc<AtomicUsize>>,
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
            if let Some(shutdowns) = &self.shutdowns {
                shutdowns.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct FakeTransport {
        configuration: Option<VideoTransportConfiguration>,
        video_codec: Option<VideoCodec>,
        sender_shutdowns: Option<Arc<AtomicUsize>>,
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
                video_codec: VideoCodec::Vp8,
                sender: Box::new(FakeSender {
                    feedback,
                    playout_delays: Some(self.playout_delays.clone()),
                    shutdowns: None,
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
            Ok(fake_transport(
                with_audio,
                self.video_codec.unwrap_or(VideoCodec::Vp8),
                self.sender_shutdowns.clone(),
            ))
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

    #[derive(Debug, Default)]
    struct PendingTransport {
        entered: bool,
        stops: u32,
    }

    #[async_trait]
    impl VideoTransportNegotiator for PendingTransport {
        async fn negotiate_video(
            &mut self,
            _configuration: VideoTransportConfiguration,
        ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
            self.entered = true;
            std::future::pending().await
        }

        async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
            self.stops += 1;
            Ok(())
        }
    }

    fn fake_transport(
        with_audio: bool,
        video_codec: VideoCodec,
        sender_shutdowns: Option<Arc<AtomicUsize>>,
    ) -> NegotiatedVideoTransport {
        let (feedback, receiver) = watch::channel(VideoTransportFeedbackSnapshot::default());
        NegotiatedVideoTransport {
            video_codec,
            sender: Box::new(FakeSender {
                feedback: feedback.clone(),
                playout_delays: None,
                shutdowns: sender_shutdowns,
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

    #[test]
    fn reported_frame_loss_includes_each_bounded_stage() {
        let graph = MediaGraphStatistics {
            raw_frames_dropped: 2,
            dropped_frames: 3,
            ..MediaGraphStatistics::default()
        };
        let sender = VideoSenderStatistics {
            dropped_frames: 5,
            ..VideoSenderStatistics::default()
        };
        assert_eq!(total_dropped_frames(&graph, &sender), 10);

        let saturated = MediaGraphStatistics {
            raw_frames_dropped: u64::MAX,
            ..MediaGraphStatistics::default()
        };
        assert_eq!(total_dropped_frames(&saturated, &sender), u64::MAX);
    }

    #[test]
    fn va_policy_offers_only_its_h264_encoder() {
        let policy = VideoEncoderPolicy::VaH264 {
            render_node: PathBuf::from("/dev/dri/renderD128"),
            render_device: test_render_device(),
            raw_layouts: vec![RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9)],
            minimum_bitrate: 0,
            maximum_bitrate: u64::MAX,
        };
        assert_eq!(policy.offer(), VideoOffer::H264Only);
        assert!(policy.encoder(VideoCodec::Vp8).is_err());
        let encoder = policy.encoder(VideoCodec::H264).unwrap();
        assert_eq!(encoder.codec(), VideoCodec::H264);
        assert_eq!(
            encoder.render_node(),
            Some(std::path::Path::new("/dev/dri/renderD128"))
        );
    }

    #[test]
    fn va_policy_requires_its_selected_render_device() {
        let policy = VideoEncoderPolicy::VaH264 {
            render_node: PathBuf::from("/dev/dri/renderD128"),
            render_device: test_render_device(),
            raw_layouts: vec![RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9)],
            minimum_bitrate: 0,
            maximum_bitrate: u64::MAX,
        };
        policy
            .validate_video_target(Some(test_render_device()))
            .unwrap();
        assert!(policy.validate_video_target(None).is_err());
        assert!(policy
            .validate_video_target(Some(RenderDeviceIdentity {
                major: 226,
                minor: 129,
            }))
            .is_err());
    }

    #[tokio::test]
    async fn mismatched_render_device_is_rejected_before_transport() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (video_output, video_receiver) = mpsc::channel(4);
        let (_audio_output, audio_receiver) = mpsc::channel(1);
        let graph = FakeGraph::video(video_output);
        let mut media = ChromiacastMediaSession::with_graph_outputs(
            session_id.into(),
            7,
            VideoEncoderPolicy::VaH264 {
                render_node: PathBuf::from("/dev/dri/renderD128"),
                render_device: test_render_device(),
                raw_layouts: vec![RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9)],
                minimum_bitrate: 0,
                maximum_bitrate: u64::MAX,
            },
            Box::new(graph),
            video_receiver,
            audio_receiver,
        );
        media.complete_preparation(capabilities()).unwrap();
        let mut target = target_on_render_device(session_id, 1);
        target.render_device.as_mut().unwrap().minor += 1;
        let mut transport = FakeTransport::default();

        assert!(matches!(
            media
                .configure(remote(), vec![target], configuration(), 1, &mut transport,)
                .await,
            Err(MediaSessionError::InvalidRequest(_))
        ));
        assert!(transport.configuration.is_none());
        assert_eq!(media.state, SessionState::Prepared);
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_va_bitrates_are_rejected_before_transport() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        for (minimum_bitrate, maximum_bitrate, rejected_limit) in
            [(3_000_000, u64::MAX, "3000000"), (0, 1_000_000, "1000000")]
        {
            let (video_output, video_receiver) = mpsc::channel(4);
            let (_audio_output, audio_receiver) = mpsc::channel(1);
            let mut media = ChromiacastMediaSession::with_graph_outputs(
                session_id.into(),
                7,
                VideoEncoderPolicy::VaH264 {
                    render_node: PathBuf::from("/dev/dri/renderD128"),
                    render_device: test_render_device(),
                    raw_layouts: vec![RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9)],
                    minimum_bitrate,
                    maximum_bitrate,
                },
                Box::new(FakeGraph::video(video_output)),
                video_receiver,
                audio_receiver,
            );
            media.complete_preparation(capabilities()).unwrap();
            let mut transport = FakeTransport::default();

            let error = media
                .configure(
                    remote(),
                    vec![target_on_render_device(session_id, 1)],
                    configuration(),
                    1,
                    &mut transport,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                MediaSessionError::InvalidRequest(message) if message.contains(rejected_limit)
            ));
            assert!(transport.configuration.is_none());
            assert_eq!(media.state, SessionState::Prepared);
            media.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejected_encoder_selection_closes_the_negotiated_sender() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (video_output, video_receiver) = mpsc::channel(4);
        let (_audio_output, audio_receiver) = mpsc::channel(1);
        let graph = FakeGraph::video(video_output);
        let mut media = ChromiacastMediaSession::with_graph_outputs(
            session_id.into(),
            7,
            VideoEncoderPolicy::VaH264 {
                render_node: PathBuf::from("/dev/dri/renderD128"),
                render_device: test_render_device(),
                raw_layouts: vec![RawVideoLayout::dma_buf(u32::from_le_bytes(*b"AR24"), 9)],
                minimum_bitrate: 0,
                maximum_bitrate: u64::MAX,
            },
            Box::new(graph),
            video_receiver,
            audio_receiver,
        );
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport {
            video_codec: Some(VideoCodec::Vp8),
            sender_shutdowns: Some(shutdowns.clone()),
            ..FakeTransport::default()
        };
        media.complete_preparation(capabilities()).unwrap();

        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![target_on_render_device(session_id, 1)],
                    configuration(),
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::Graph(_))
        ));
        assert_eq!(shutdowns.load(Ordering::Relaxed), 1);
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
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
                framerate_numerator: chromecast_video_cadence().numerator.get(),
                framerate_denominator: 1,
                bitrate: 2_000_000,
                target_playout_delay: INITIAL_PLAYOUT_DELAY,
                offer: VideoOffer::H264Preferred,
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
    async fn abort_media_releases_local_owners_without_waiting_for_the_receiver() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        media.complete_preparation(capabilities()).unwrap();
        let mut transport = FakeTransport::default();
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

        media.abort_media(1).await.unwrap();

        assert_eq!(transport.stops, 0);
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_video_configuration_still_stops_the_sender() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        media.complete_preparation(capabilities()).unwrap();
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport {
            sender_shutdowns: Some(shutdowns.clone()),
            ..FakeTransport::default()
        };

        // Poll on this task so the actor cannot reply before ConfigureMedia
        // is cancelled after sending it the generation.
        {
            let configure = media.configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            );
            tokio::pin!(configure);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(configure.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }

        assert!(media.active_generation().video_sender_may_own_generation());
        assert!(!media.active_generation().is_ready());
        assert!(matches!(
            media.start(1).await,
            Err(MediaSessionError::Graph(_))
        ));
        media.stop_media(1, &mut transport).await.unwrap();
        assert_eq!(shutdowns.load(Ordering::Relaxed), 1);
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_audio_configuration_still_stops_the_sender() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (video_output, video_receiver) = mpsc::channel(4);
        let (audio_output, audio_receiver) = mpsc::channel(4);
        let graph = FakeGraph::audio(video_output, audio_output);
        let mut media = ChromiacastMediaSession::with_graph_outputs(
            session_id.into(),
            7,
            VideoEncoderPolicy::Software,
            Box::new(graph),
            video_receiver,
            audio_receiver,
        );
        media.complete_preparation(audio_capabilities()).unwrap();
        let mut transport = FakeTransport::default();

        {
            let configure = media.configure(
                audio_remotes(),
                vec![target(session_id, 1), audio_target(session_id, 1)],
                audio_configuration(),
                1,
                &mut transport,
            );
            tokio::pin!(configure);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(configure.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }

        assert!(media.active_generation().audio_sender_may_own_generation());
        assert!(!media.active_generation().video_sender_may_own_generation());
        assert!(!media.active_generation().is_ready());
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn configured_mode_accepts_the_realized_kms_refresh() {
        for refresh in [59_951, 60_049] {
            let session_id = "12345678-1234-1234-1234-123456789abc";
            let (output, receiver) = mpsc::channel(4);
            let graph = FakeGraph::video(output);
            let mut media = ChromiacastMediaSession::with_graph(
                session_id.into(),
                7,
                Box::new(graph),
                receiver,
            );
            let mut transport = FakeTransport::default();
            media.complete_preparation(capabilities()).unwrap();
            let mut realized = configuration();
            realized.mode.refresh_millihz = refresh;

            media
                .configure(
                    remote(),
                    vec![target(session_id, 1)],
                    realized,
                    1,
                    &mut transport,
                )
                .await
                .unwrap();

            media.stop_media(1, &mut transport).await.unwrap();
            media.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn configured_mode_rejects_a_distinct_refresh() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        let mut distinct = configuration();
        distinct.mode.refresh_millihz = 59_000;

        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![target(session_id, 1)],
                    distinct,
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::InvalidRequest(_))
        ));

        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn capture_sampling_cadence_is_independent_of_display_presentation() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        let mut capture_target = target(session_id, 1);
        capture_target.caps = "video/x-raw,format=BGRx,width=640,height=480,framerate=30/1".into();

        media
            .configure(
                remote(),
                vec![capture_target],
                configuration(),
                1,
                &mut transport,
            )
            .await
            .unwrap();

        assert_eq!(configuration().mode.refresh_millihz, 60_000);
        assert_eq!(
            transport
                .configuration
                .as_ref()
                .unwrap()
                .framerate_numerator,
            30
        );
        media.stop_media(1, &mut transport).await.unwrap();
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn capture_source_must_supply_the_selected_cadence() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        let mut capture_target = target(session_id, 1);
        capture_target.caps = "video/x-raw,format=BGRx,width=640,height=480,framerate=24/1".into();

        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![capture_target],
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
    async fn capture_source_must_supply_the_negotiated_raw_layout() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport::default();
        media.complete_preparation(capabilities()).unwrap();
        let mut capture_target = target(session_id, 1);
        capture_target.caps = concat!(
            "video/x-raw(memory:DMABuf),format=DMA_DRM,",
            "drm-format=XR24:0x0000000000000000,",
            "width=640,height=480,framerate=60/1"
        )
        .into();

        assert!(matches!(
            media
                .configure(
                    remote(),
                    vec![capture_target],
                    configuration(),
                    1,
                    &mut transport,
                )
                .await,
            Err(MediaSessionError::InvalidRequest(_))
        ));
        assert!(transport.configuration.is_none());
        media.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn receiver_selected_h264_configures_the_software_encoder() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        let mut transport = FakeTransport {
            video_codec: Some(VideoCodec::H264),
            ..FakeTransport::default()
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

        let statistics = media
            .graph
            .statistics(NonZeroU64::new(1).unwrap())
            .await
            .unwrap();
        assert_eq!(statistics.encoder_name.as_deref(), Some("x264enc"));
        assert_eq!(statistics.video_cadence_numerator, 30);
        assert_eq!(statistics.video_cadence_denominator, 1);
        media.stop_media(1, &mut transport).await.unwrap();
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
            VideoEncoderPolicy::Software,
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
                framerate_numerator: chromecast_video_cadence().numerator.get(),
                framerate_denominator: 1,
                bitrate: 2_000_000,
                target_playout_delay: INITIAL_PLAYOUT_DELAY,
                offer: VideoOffer::H264Preferred,
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
    async fn matching_stop_remains_valid_after_transport_negotiation_failure() {
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
    async fn cancelled_transport_negotiation_still_requests_stop() {
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let (output, receiver) = mpsc::channel(4);
        let graph = FakeGraph::video(output);
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
        media.complete_preparation(capabilities()).unwrap();
        let mut transport = PendingTransport::default();

        {
            let configure = media.configure(
                remote(),
                vec![target(session_id, 1)],
                configuration(),
                1,
                &mut transport,
            );
            tokio::pin!(configure);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(configure.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }

        assert!(transport.entered);
        media.stop_media(1, &mut transport).await.unwrap();
        assert_eq!(transport.stops, 1);
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
        assert_eq!(*playout_delays.lock().unwrap(), [Duration::from_millis(99)]);

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
                raw_layouts: vec![pronk_backend_protocol::RawVideoLayout::system_memory(
                    u32::from_le_bytes(*b"XR24"),
                )],
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

    fn test_render_device() -> RenderDeviceIdentity {
        RenderDeviceIdentity {
            major: 226,
            minor: 128,
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
            render_device: None,
            caps: "video/x-raw,format=BGRx,width=640,height=480,framerate=60/1".into(),
        }
    }

    fn target_on_render_device(session_id: &str, media_generation: u64) -> PipeWireTarget {
        PipeWireTarget {
            render_device: Some(test_render_device()),
            ..target(session_id, media_generation)
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
