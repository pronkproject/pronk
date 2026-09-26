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
    phase: MediaPhase,
    encoder_policy: VideoEncoderPolicy,
    graph: Box<dyn MediaGraphPort>,
}

#[derive(Debug)]
enum MediaPhase {
    Created {
        senders: SenderActors,
    },
    Prepared {
        capabilities: DeviceCapabilities,
        completed: Option<NonZeroU64>,
        senders: SenderActors,
    },
    Active {
        capabilities: DeviceCapabilities,
        generation: Box<ActiveGeneration>,
        state: ActiveState,
        senders: SenderActors,
    },
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveState {
    Configured,
    Streaming,
    Suspended,
}

impl MediaPhase {
    fn state(&self) -> SessionState {
        match self {
            Self::Created { .. } => SessionState::Created,
            Self::Prepared { .. } => SessionState::Prepared,
            Self::Active {
                state: ActiveState::Configured,
                ..
            } => SessionState::Configured,
            Self::Active {
                state: ActiveState::Streaming,
                ..
            } => SessionState::Streaming,
            Self::Active {
                state: ActiveState::Suspended,
                ..
            } => SessionState::Suspended,
            Self::Stopped => SessionState::Stopped,
        }
    }

    fn active(&self) -> Option<&ActiveGeneration> {
        match self {
            Self::Active { generation, .. } => Some(generation),
            _ => None,
        }
    }

    fn active_mut(&mut self) -> Option<&mut ActiveGeneration> {
        match self {
            Self::Active { generation, .. } => Some(generation),
            _ => None,
        }
    }

    fn completed(&self) -> Option<NonZeroU64> {
        match self {
            Self::Prepared { completed, .. } => *completed,
            _ => None,
        }
    }

    fn senders(&self) -> Option<&SenderActors> {
        match self {
            Self::Created { senders }
            | Self::Prepared { senders, .. }
            | Self::Active { senders, .. } => Some(senders),
            Self::Stopped => None,
        }
    }

    fn set_active_state(&mut self, next: ActiveState) {
        match self {
            Self::Active { state, .. } => *state = next,
            _ => unreachable!("only an active media phase can change playback state"),
        }
    }
}

#[derive(Debug)]
struct SenderActors {
    video: VideoSenderActor,
    audio: AudioSenderActor,
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
        self.phase
            .active()
            .expect("configured media session owns an active generation")
    }

    fn active_generation_mut(&mut self) -> &mut ActiveGeneration {
        self.phase
            .active_mut()
            .expect("configured media session owns an active generation")
    }

    fn state(&self) -> SessionState {
        self.phase.state()
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
            phase: MediaPhase::Created {
                senders: SenderActors {
                    video: VideoSenderActor::spawn(video_output),
                    audio: AudioSenderActor::spawn(audio_output),
                },
            },
            encoder_policy,
            graph,
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

    pub(crate) fn session_generation(&self) -> u64 {
        self.session_generation
    }

    pub(crate) fn complete_preparation(
        &mut self,
        capabilities: DeviceCapabilities,
    ) -> Result<(), MediaSessionError> {
        if self.state() != SessionState::Created {
            return Err(MediaSessionError::Transition {
                operation: "Prepare",
                state: self.state(),
            });
        }
        capabilities
            .validate()
            .map_err(|error| MediaSessionError::InvalidRequest(error.to_string()))?;
        let MediaPhase::Created { senders } =
            std::mem::replace(&mut self.phase, MediaPhase::Stopped)
        else {
            unreachable!("Prepare checked the created phase");
        };
        self.phase = MediaPhase::Prepared {
            capabilities,
            completed: None,
            senders,
        };
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
        if self.state() != SessionState::Prepared {
            return Err(MediaSessionError::Transition {
                operation: "ConfigureMedia",
                state: self.state(),
            });
        }
        let generation = NonZeroU64::new(media_generation).ok_or_else(|| {
            MediaSessionError::InvalidRequest("media generation must be nonzero".into())
        })?;
        if self
            .phase
            .completed()
            .is_some_and(|completed| generation <= completed)
        {
            return Err(MediaSessionError::InvalidRequest(format!(
                "media generation {generation} is not newer than completed generation {:?}",
                self.phase.completed()
            )));
        }
        let MediaPhase::Prepared { capabilities, .. } = &self.phase else {
            unreachable!("ConfigureMedia checked the prepared phase");
        };
        let (graph_configuration, transport_configuration) = graph_configuration(
            &self.session_id,
            capabilities,
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
        let MediaPhase::Prepared {
            capabilities,
            senders,
            ..
        } = std::mem::replace(&mut self.phase, MediaPhase::Stopped)
        else {
            unreachable!("ConfigureMedia checked the prepared phase");
        };
        self.phase = MediaPhase::Active {
            capabilities,
            generation: Box::new(ActiveGeneration::new(
                generation,
                audio_enabled,
                video_bitrate,
            )),
            state: ActiveState::Configured,
            senders,
        };

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
        if self.phase.active().map(|active| active.id) != Some(generation)
            || !self.active_generation().is_ready()
        {
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
        self.phase.set_active_state(ActiveState::Streaming);
        Ok(())
    }

    pub(crate) async fn suspend(&mut self) -> Result<(), MediaSessionError> {
        if self.state() != SessionState::Streaming {
            return Err(MediaSessionError::Transition {
                operation: "Suspend",
                state: self.state(),
            });
        }
        let generation =
            self.phase.active().map(|active| active.id).ok_or_else(|| {
                MediaSessionError::Graph("active media generation is missing".into())
            })?;
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
        self.phase.set_active_state(ActiveState::Suspended);
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
        self.phase.set_active_state(ActiveState::Streaming);
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
        if self.state() == SessionState::Prepared && self.phase.completed() == Some(generation) {
            return Ok(());
        }
        if !matches!(
            self.state(),
            SessionState::Configured | SessionState::Streaming | SessionState::Suspended
        ) {
            return Err(MediaSessionError::Transition {
                operation: "StopMedia",
                state: self.state(),
            });
        }
        self.require_matching_generation("StopMedia", generation)?;
        let active = self.active_generation();
        let graph_may_own_generation = active.graph_may_own_generation();
        let audio_sender_may_own_generation = active.audio_sender_may_own_generation();
        let sender_may_own_generation = active.video_sender_may_own_generation();
        let graph = &mut self.graph;
        let MediaPhase::Active { senders, .. } = &self.phase else {
            unreachable!("StopMedia checked the active phase");
        };
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
                    senders
                        .audio
                        .stop(generation)
                        .await
                        .map(|_| ())
                        .map_err(MediaSessionError::from)
                } else {
                    Ok(())
                }
            },
            async {
                if sender_may_own_generation {
                    senders
                        .video
                        .stop(generation)
                        .await
                        .map(|_| ())
                        .map_err(MediaSessionError::from)
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
        let MediaPhase::Active {
            capabilities,
            senders,
            ..
        } = std::mem::replace(&mut self.phase, MediaPhase::Stopped)
        else {
            unreachable!("StopMedia checked the active phase");
        };
        self.phase = MediaPhase::Prepared {
            capabilities,
            completed: Some(generation),
            senders,
        };
        graph_result
            .and(audio_sender_result)
            .and(sender_result)
            .and(transport_result)
    }

    pub(crate) async fn statistics(&mut self) -> Result<SessionStatistics, MediaSessionError> {
        if !self.phase.active().is_some_and(|active| active.is_ready())
            || !matches!(
                self.state(),
                SessionState::Configured | SessionState::Streaming | SessionState::Suspended
            )
        {
            return Err(MediaSessionError::Transition {
                operation: "GetStatistics",
                state: self.state(),
            });
        }
        let generation =
            self.phase.active().map(|active| active.id).ok_or_else(|| {
                MediaSessionError::Graph("active media generation is missing".into())
            })?;
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
        let senders = match std::mem::replace(&mut self.phase, MediaPhase::Stopped) {
            MediaPhase::Created { senders }
            | MediaPhase::Prepared { senders, .. }
            | MediaPhase::Active { senders, .. } => Some(senders),
            MediaPhase::Stopped => None,
        };
        let (audio_sender, sender) = match senders {
            Some(senders) => (Some(senders.audio), Some(senders.video)),
            None => (None, None),
        };
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
        if self.state() != required_state {
            return Err(MediaSessionError::Transition {
                operation,
                state: self.state(),
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
        if self.phase.active().map(|active| active.id) == Some(generation) {
            Ok(())
        } else {
            Err(MediaSessionError::InvalidRequest(format!(
                "{operation} generation {generation} does not match active generation {:?}",
                self.phase.active().map(|active| active.id)
            )))
        }
    }

    fn sender(&self) -> Result<&VideoSenderActor, MediaSessionError> {
        self.phase
            .senders()
            .map(|senders| &senders.video)
            .ok_or_else(|| MediaSessionError::Transport("video sender actor is shut down".into()))
    }

    fn audio_sender(&self) -> Result<&AudioSenderActor, MediaSessionError> {
        self.phase
            .senders()
            .map(|senders| &senders.audio)
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
mod tests;
