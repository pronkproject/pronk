use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use chromiacast::{
    AudioCodec, AudioStreamConfig, CastApp, CastConnection, EncodedFrame, EnqueueError,
    FrameDependency, Framerate, Offer, Resolution, SenderEvent, SenderSession, StreamHandle,
    StreamType, UdpTransport, VideoCodec, VideoStreamConfig, APP_MIRRORING,
};
use pronk_media::{EncodedAudioPacket, EncodedVideoAccessUnit, VideoFrameDependency};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::transport::{
    AudioSendOutcome, AudioSenderPort, NegotiatedVideoTransport, VideoSendOutcome, VideoSenderPort,
    VideoTransportConfiguration, VideoTransportError, VideoTransportFeedbackSnapshot,
    VideoTransportPressure,
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn negotiate_video(
    connection: &CastConnection,
    configuration: VideoTransportConfiguration,
) -> Result<(CastApp, NegotiatedVideoTransport), VideoTransportError> {
    let app = connection
        .launch(APP_MIRRORING)
        .await
        .map_err(|error| VideoTransportError::new(format!("launch mirroring app: {error}")))?;
    let result = negotiate_launched_video(connection, &app, configuration).await;
    match result {
        Ok(sender) => Ok((app, sender)),
        Err(error) => {
            let _ = connection.stop(&app).await;
            Err(error)
        }
    }
}

async fn negotiate_launched_video(
    connection: &CastConnection,
    app: &CastApp,
    configuration: VideoTransportConfiguration,
) -> Result<NegotiatedVideoTransport, VideoTransportError> {
    tracing::info!(
        width = configuration.width,
        height = configuration.height,
        framerate_numerator = configuration.framerate_numerator,
        framerate_denominator = configuration.framerate_denominator,
        bitrate = configuration.bitrate,
        target_playout_delay_milliseconds = configuration.target_playout_delay.as_millis(),
        audio = configuration.audio.is_some(),
        "offering Cast media configuration"
    );
    let offer = build_offer(configuration);
    let answer = connection
        .exchange_offer(&offer, app)
        .await
        .map_err(|error| VideoTransportError::new(format!("exchange Cast OFFER: {error}")))?;
    tracing::info!(
        constraints = ?answer.constraints,
        display = ?answer.display,
        "received Cast streaming constraints"
    );
    validate_answer_constraints(&answer, configuration)?;
    let minimum_bitrate = answer
        .constraints
        .as_ref()
        .and_then(|constraints| constraints.video.as_ref())
        .and_then(|video| video.min_bit_rate)
        .and_then(std::num::NonZeroU32::new);

    let endpoint = connection.remote_address();
    let bind_ip = if endpoint.is_ipv6() {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    };
    let transport = UdpTransport::bind(SocketAddr::new(bind_ip, 0))
        .await
        .map_err(|error| VideoTransportError::new(format!("bind Cast UDP transport: {error}")))?;
    let (session, events) = SenderSession::start_address(&offer, &answer, endpoint, transport)
        .await
        .map_err(|error| VideoTransportError::new(format!("start Cast sender: {error}")))?;
    let video = session
        .video()
        .ok_or_else(|| VideoTransportError::new("Cast receiver did not accept the video stream"))?;
    let audio = match configuration.audio {
        Some(_) => Some(session.audio().ok_or_else(|| {
            VideoTransportError::new("Cast receiver did not accept the requested audio stream")
        })?),
        None => None,
    };
    let maximum_playout_delay = maximum_playout_delay(&answer, configuration.audio.is_some());
    let (sender, audio_sender, feedback) =
        ChromiacastVideoSender::new(session, video, audio, events, maximum_playout_delay);
    Ok(NegotiatedVideoTransport {
        sender: Box::new(sender),
        audio_sender: audio_sender.map(|sender| Box::new(sender) as Box<dyn AudioSenderPort>),
        feedback,
        minimum_bitrate,
    })
}

fn build_offer(configuration: VideoTransportConfiguration) -> Offer {
    let offer_builder = Offer::builder();
    let offer_builder = match configuration.audio {
        Some(audio) => offer_builder.audio(AudioStreamConfig {
            codec: AudioCodec::Opus,
            bit_rate: audio.bitrate,
            sample_rate: audio.sample_rate,
            channels: audio.channels,
            target_delay: configuration.target_playout_delay,
        }),
        None => offer_builder,
    };
    offer_builder
        .video(VideoStreamConfig {
            codec: VideoCodec::H264,
            max_bit_rate: configuration.bitrate,
            max_frame_rate: Framerate::new(
                configuration.framerate_numerator,
                configuration.framerate_denominator,
            ),
            resolutions: vec![Resolution::new(configuration.width, configuration.height)],
            target_delay: configuration.target_playout_delay,
        })
        .build()
}

fn validate_answer_constraints(
    answer: &chromiacast::Answer,
    configuration: VideoTransportConfiguration,
) -> Result<(), VideoTransportError> {
    if let Some(configured) = configuration.audio {
        if let Some(audio) = answer
            .constraints
            .as_ref()
            .and_then(|constraints| constraints.audio.as_ref())
        {
            if audio
                .max_sample_rate
                .is_some_and(|maximum| configured.sample_rate > maximum)
                || audio
                    .max_channels
                    .is_some_and(|maximum| configured.channels > maximum)
                || audio
                    .min_bit_rate
                    .is_some_and(|minimum| configured.bitrate < minimum)
                || audio
                    .max_bit_rate
                    .is_some_and(|maximum| configured.bitrate > maximum)
                || audio.max_delay.is_some_and(|maximum_ms| {
                    configuration.target_playout_delay
                        > Duration::from_millis(u64::from(maximum_ms))
                })
            {
                return Err(VideoTransportError::new(
                    "configured audio format or bitrate violates the Cast ANSWER constraints",
                ));
            }
        }
    }
    let Some(video) = answer
        .constraints
        .as_ref()
        .and_then(|constraints| constraints.video.as_ref())
    else {
        return Ok(());
    };
    if video.min_resolution.is_some_and(|minimum| {
        configuration.width < minimum.width || configuration.height < minimum.height
    }) || video.max_dimensions.is_some_and(|maximum| {
        configuration.width > maximum.width
            || configuration.height > maximum.height
            || maximum.frame_rate.is_some_and(|maximum| {
                frame_rate_exceeds(
                    configuration.framerate_numerator,
                    configuration.framerate_denominator,
                    maximum,
                )
            })
    }) || video
        .min_bit_rate
        .is_some_and(|minimum| configuration.bitrate < minimum)
        || video
            .max_bit_rate
            .is_some_and(|maximum| configuration.bitrate > maximum)
        || video.max_delay.is_some_and(|maximum_ms| {
            configuration.target_playout_delay > Duration::from_millis(u64::from(maximum_ms))
        })
    {
        return Err(VideoTransportError::new(
            "configured video mode or bitrate violates the Cast ANSWER constraints",
        ));
    }
    if let Some(maximum) = video.max_pixels_per_second {
        let pixels_per_second = f64::from(configuration.width)
            * f64::from(configuration.height)
            * f64::from(configuration.framerate_numerator)
            / f64::from(configuration.framerate_denominator);
        if pixels_per_second > maximum {
            return Err(VideoTransportError::new(
                "configured video rate exceeds the Cast ANSWER pixel-rate constraint",
            ));
        }
    }
    Ok(())
}

fn frame_rate_exceeds(numerator: u32, denominator: u32, maximum: Framerate) -> bool {
    u64::from(numerator) * u64::from(maximum.denominator)
        > u64::from(maximum.numerator) * u64::from(denominator)
}

fn maximum_playout_delay(answer: &chromiacast::Answer, audio_enabled: bool) -> Option<Duration> {
    let constraints = answer.constraints.as_ref()?;
    let video = constraints.video.as_ref().and_then(|video| video.max_delay);
    let audio = audio_enabled
        .then(|| constraints.audio.as_ref().and_then(|audio| audio.max_delay))
        .flatten();
    video
        .into_iter()
        .chain(audio)
        .min()
        .map(|milliseconds| Duration::from_millis(u64::from(milliseconds)))
}

struct ChromiacastVideoSender {
    session: Option<SenderSession>,
    video: StreamHandle,
    maximum_playout_delay: Option<Duration>,
    terminal: watch::Receiver<Option<VideoTransportError>>,
    event_task: Option<JoinHandle<()>>,
}

impl ChromiacastVideoSender {
    fn new(
        session: SenderSession,
        video: StreamHandle,
        audio: Option<StreamHandle>,
        mut events: tokio::sync::mpsc::Receiver<SenderEvent>,
        maximum_playout_delay: Option<Duration>,
    ) -> (
        Self,
        Option<ChromiacastAudioSender>,
        watch::Receiver<VideoTransportFeedbackSnapshot>,
    ) {
        let (terminal_tx, terminal) = watch::channel(None);
        let (feedback_tx, feedback) = watch::channel(VideoTransportFeedbackSnapshot::default());
        let event_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if !project_sender_event(event, &terminal_tx, &feedback_tx) {
                    return;
                }
            }
        });
        let audio_sender = audio.map(|audio| ChromiacastAudioSender {
            audio,
            terminal: terminal.clone(),
        });
        (
            Self {
                session: Some(session),
                video,
                maximum_playout_delay,
                terminal,
                event_task: Some(event_task),
            },
            audio_sender,
            feedback,
        )
    }

    fn terminal_error(&self) -> Option<VideoTransportError> {
        self.terminal.borrow().clone()
    }
}

impl Drop for ChromiacastVideoSender {
    fn drop(&mut self) {
        if let Some(task) = self.event_task.take() {
            // `shutdown` is the orderly path. A dropped transport must not
            // detach an event pump which still owns session-facing channels.
            task.abort();
        }
    }
}

fn project_sender_event(
    event: SenderEvent,
    terminal: &watch::Sender<Option<VideoTransportError>>,
    feedback: &watch::Sender<VideoTransportFeedbackSnapshot>,
) -> bool {
    match event {
        SenderEvent::FrameAcked {
            stream: StreamType::Video,
            ..
        } => feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_frames = snapshot.acknowledged_frames.saturating_add(1);
        }),
        SenderEvent::FrameAcked {
            stream: StreamType::Audio,
            ..
        } => feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_audio_packets =
                snapshot.acknowledged_audio_packets.saturating_add(1);
        }),
        SenderEvent::NeedsKeyFrame {
            stream: StreamType::Video,
        }
        | SenderEvent::PictureLoss {
            stream: StreamType::Video,
        } => feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.key_frame_requests = snapshot.key_frame_requests.saturating_add(1);
        }),
        SenderEvent::StatisticsUpdated(statistics) => {
            let audio = statistics.audio;
            let acknowledged_audio_packets = audio.as_ref().map(|audio| audio.frames_acked);
            let video = statistics.video;
            if let Some(video) = video.as_ref() {
                tracing::debug!(
                    acknowledged_frames = video.frames_acked,
                    in_flight_frames = video.in_flight_frames,
                    in_flight_media_milliseconds = video.in_flight_media_duration.as_millis(),
                    acceptable_in_flight_milliseconds =
                        video.max_acceptable_in_flight_duration.as_millis(),
                    rtt_milliseconds = ?video.current_rtt.map(|duration| duration.as_millis()),
                    video_playout_delay_milliseconds =
                        ?video.receiver_playout_delay.map(|duration| duration.as_millis()),
                    audio_playout_delay_milliseconds = ?audio
                        .as_ref()
                        .and_then(|audio| audio.receiver_playout_delay)
                        .map(|duration| duration.as_millis()),
                    packets_sent = video.packets_sent,
                    packets_retransmitted = video.packets_retransmitted,
                    frames_dropped_or_skipped = video.frames_dropped_or_skipped,
                    nack_count = video.nack_count,
                    fraction_lost = ?video.fraction_lost,
                    jitter_ticks = ?video.jitter,
                    "received Cast transport statistics"
                );
                // A target applies to the synchronized session, so video
                // feedback alone must not confirm it while audio is still on
                // the previous value.
                let receiver_playout_delay = match audio.as_ref() {
                    None => video.receiver_playout_delay,
                    Some(audio) if audio.receiver_playout_delay == video.receiver_playout_delay => {
                        video.receiver_playout_delay
                    }
                    Some(_) => None,
                };
                feedback.send_modify(|snapshot| {
                    snapshot.revision = snapshot.revision.saturating_add(1);
                    snapshot.acknowledged_frames = video.frames_acked;
                    if let Some(audio) = acknowledged_audio_packets {
                        snapshot.acknowledged_audio_packets = audio;
                    }
                    snapshot.pressure = Some(VideoTransportPressure {
                        in_flight_frames: video.in_flight_frames,
                        in_flight_media_duration: video.in_flight_media_duration,
                        max_acceptable_in_flight_duration: video.max_acceptable_in_flight_duration,
                        current_rtt: video.current_rtt,
                        receiver_playout_delay,
                        nack_count: video.nack_count,
                        frames_dropped_or_skipped: video.frames_dropped_or_skipped,
                        fraction_lost: video.fraction_lost,
                    });
                });
            }
            if video.is_none() {
                if let Some(audio) = acknowledged_audio_packets {
                    feedback.send_modify(|snapshot| {
                        snapshot.revision = snapshot.revision.saturating_add(1);
                        snapshot.acknowledged_audio_packets = audio;
                    });
                }
            }
        }
        SenderEvent::FatalError(error) => {
            publish_terminal_sender_error(
                VideoTransportError::new(format!("Cast sender failed: {error}")),
                terminal,
                feedback,
            );
            return false;
        }
        SenderEvent::ReceiverTimedOut => {
            publish_terminal_sender_error(
                VideoTransportError::new("Cast receiver stopped acknowledging media"),
                terminal,
                feedback,
            );
            return false;
        }
        _ => {}
    }
    true
}

struct ChromiacastAudioSender {
    audio: StreamHandle,
    terminal: watch::Receiver<Option<VideoTransportError>>,
}

impl std::fmt::Debug for ChromiacastAudioSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChromiacastAudioSender")
            .field("terminal", &self.terminal.borrow())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AudioSenderPort for ChromiacastAudioSender {
    async fn send(
        &mut self,
        packet: EncodedAudioPacket,
    ) -> Result<AudioSendOutcome, VideoTransportError> {
        if let Some(error) = self.terminal.borrow().clone() {
            return Err(error);
        }
        let outcome = match self.audio.send(cast_audio_packet(packet)).await {
            Ok(_) => AudioSendOutcome::Accepted,
            Err(EnqueueError::ReachedIdSpanLimit) => AudioSendOutcome::Congested,
            Err(error) => {
                return Err(VideoTransportError::new(format!(
                    "enqueue Cast audio: {error}"
                )))
            }
        };
        if let Some(error) = self.terminal.borrow().clone() {
            return Err(error);
        }
        Ok(outcome)
    }

    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

fn publish_terminal_sender_error(
    error: VideoTransportError,
    terminal: &watch::Sender<Option<VideoTransportError>>,
    feedback: &watch::Sender<VideoTransportFeedbackSnapshot>,
) {
    terminal.send_replace(Some(error.clone()));
    feedback.send_modify(|snapshot| {
        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.terminal_error = Some(error);
    });
}

impl std::fmt::Debug for ChromiacastVideoSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChromiacastVideoSender")
            .field("terminal", &self.terminal.borrow())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl VideoSenderPort for ChromiacastVideoSender {
    fn supports_target_playout_delay_updates(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(SenderSession::supports_target_playout_delay_updates)
    }

    fn maximum_target_playout_delay(&self) -> Option<Duration> {
        self.maximum_playout_delay
    }

    async fn set_target_playout_delay(
        &mut self,
        delay: Duration,
    ) -> Result<(), VideoTransportError> {
        self.session
            .as_ref()
            .ok_or_else(|| VideoTransportError::new("Cast sender session is stopped"))?
            .set_target_playout_delay(delay)
            .await
            .map_err(|error| {
                VideoTransportError::new(format!("set Cast target playout delay: {error}"))
            })
    }

    async fn send(
        &mut self,
        access_unit: EncodedVideoAccessUnit,
    ) -> Result<VideoSendOutcome, VideoTransportError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        let outcome = match self.video.send(cast_frame(access_unit)).await {
            Ok(_) => VideoSendOutcome::Accepted,
            Err(EnqueueError::ReachedIdSpanLimit) => VideoSendOutcome::Congested,
            Err(error) => {
                return Err(VideoTransportError::new(format!(
                    "enqueue Cast video: {error}"
                )))
            }
        };
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        Ok(outcome)
    }

    async fn shutdown(mut self: Box<Self>) -> Result<(), VideoTransportError> {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        let sender_result = match self.session.take() {
            Some(session) => tokio::time::timeout(SHUTDOWN_TIMEOUT, session.shutdown())
                .await
                .map_err(|_| VideoTransportError::new("timed out stopping Cast sender"))
                .and_then(|result| {
                    result.map_err(|error| {
                        VideoTransportError::new(format!("stop Cast sender: {error}"))
                    })
                }),
            None => Ok(()),
        };
        let event_result = match self.event_task.take() {
            Some(task) => {
                join_sender_event_task(
                    task,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await
            }
            None => Ok(()),
        };
        sender_result.and(event_result)
    }
}

async fn join_sender_event_task(
    mut task: JoinHandle<()>,
    remaining: Duration,
) -> Result<(), VideoTransportError> {
    match tokio::time::timeout(remaining, &mut task).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(VideoTransportError::new(format!(
            "join Cast sender event task: {error}"
        ))),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(VideoTransportError::new(
                "timed out stopping Cast sender event task",
            ))
        }
    }
}

fn cast_frame(access_unit: EncodedVideoAccessUnit) -> EncodedFrame {
    let dependency = match access_unit.dependency {
        VideoFrameDependency::KeyFrame => FrameDependency::KeyFrame,
        VideoFrameDependency::Delta => FrameDependency::Delta,
    };
    EncodedFrame::new(
        dependency,
        access_unit.data.into(),
        access_unit.media_timestamp,
        access_unit.reference_time,
    )
    .with_duration(access_unit.duration)
}

fn cast_audio_packet(packet: EncodedAudioPacket) -> EncodedFrame {
    EncodedFrame::new(
        FrameDependency::KeyFrame,
        packet.data.into(),
        packet.media_timestamp,
        packet.reference_time,
    )
    .with_duration(packet.duration)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::time::{Duration, Instant};

    use chromiacast::{
        Answer, FrameId, SenderEvent, SessionStatistics, StreamStatistics, StreamType,
    };
    use pronk_media::{EncodedAudioPacket, EncodedVideoAccessUnit, VideoFrameDependency};

    use super::{
        build_offer, cast_audio_packet, cast_frame, join_sender_event_task, maximum_playout_delay,
        project_sender_event, validate_answer_constraints,
    };
    use crate::transport::{
        AudioTransportConfiguration, VideoTransportConfiguration, VideoTransportError,
        VideoTransportFeedbackSnapshot,
    };

    #[test]
    fn answer_constraints_fail_closed() {
        let answer: Answer = serde_json::from_str(
            r#"{
                "udpPort": 2344,
                "sendIndexes": [0],
                "ssrcs": [123],
                "constraints": {
                    "video": {
                        "maxDimensions": {"width": 1280, "height": 720},
                        "maxBitRate": 1000000
                    }
                }
            }"#,
        )
        .unwrap();
        let configuration = VideoTransportConfiguration {
            width: 1920,
            height: 1080,
            framerate_numerator: 60,
            framerate_denominator: 1,
            bitrate: 2_000_000,
            target_playout_delay: Duration::from_millis(33),
            audio: None,
        };
        assert!(validate_answer_constraints(&answer, configuration).is_err());
    }

    #[test]
    fn answer_dimension_rate_is_enforced() {
        let answer: Answer = serde_json::from_str(
            r#"{
                "udpPort": 2344,
                "sendIndexes": [0],
                "ssrcs": [123],
                "constraints": {
                    "video": {
                        "maxDimensions": {
                            "width": 3840,
                            "height": 2160,
                            "frameRate": "30"
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let mut configuration = VideoTransportConfiguration {
            width: 3840,
            height: 2160,
            framerate_numerator: 60,
            framerate_denominator: 1,
            bitrate: 8_000_000,
            target_playout_delay: Duration::from_millis(33),
            audio: None,
        };
        assert!(validate_answer_constraints(&answer, configuration).is_err());

        configuration.framerate_numerator = 30;
        assert!(validate_answer_constraints(&answer, configuration).is_ok());
    }

    #[test]
    fn desktop_offer_requests_interactive_synchronized_playout() {
        let configuration = VideoTransportConfiguration {
            width: 1920,
            height: 1080,
            framerate_numerator: 60,
            framerate_denominator: 1,
            bitrate: 2_000_000,
            target_playout_delay: Duration::from_millis(33),
            audio: Some(AudioTransportConfiguration {
                sample_rate: 48_000,
                channels: 2,
                bitrate: 128_000,
            }),
        };
        let offer = serde_json::to_value(build_offer(configuration)).unwrap();
        let streams = offer["supportedStreams"].as_array().unwrap();

        assert_eq!(streams.len(), 2);
        assert!(streams.iter().all(|stream| stream["targetDelay"] == 33));
    }

    #[test]
    fn encoded_access_unit_maps_wholesale_to_chromiacast() {
        let reference_time = Instant::now();
        let frame = cast_frame(EncodedVideoAccessUnit {
            media_generation: NonZeroU64::new(7).unwrap(),
            dependency: VideoFrameDependency::KeyFrame,
            data: vec![0, 0, 0, 1, 0x65, 0xaa],
            media_timestamp: Duration::from_millis(33),
            reference_time,
            duration: Duration::from_millis(16),
        });
        assert_eq!(frame.dependency, chromiacast::FrameDependency::KeyFrame);
        assert_eq!(frame.data.as_ref(), [0, 0, 0, 1, 0x65, 0xaa]);
        assert_eq!(frame.media_timestamp, Duration::from_millis(33));
        assert_eq!(frame.reference_time, reference_time);
        assert_eq!(frame.duration, Some(Duration::from_millis(16)));
    }

    #[test]
    fn encoded_audio_packet_maps_wholesale_to_chromiacast() {
        let reference_time = Instant::now();
        let frame = cast_audio_packet(EncodedAudioPacket {
            media_generation: NonZeroU64::new(7).unwrap(),
            data: vec![0xf8, 0xff, 0xfe],
            media_timestamp: Duration::from_millis(40),
            reference_time,
            duration: Duration::from_millis(20),
        });
        assert_eq!(frame.dependency, chromiacast::FrameDependency::KeyFrame);
        assert_eq!(frame.data.as_ref(), [0xf8, 0xff, 0xfe]);
        assert_eq!(frame.media_timestamp, Duration::from_millis(40));
        assert_eq!(frame.reference_time, reference_time);
        assert_eq!(frame.duration, Some(Duration::from_millis(20)));
    }

    #[test]
    fn answer_audio_constraints_fail_closed() {
        let answer: Answer = serde_json::from_str(
            r#"{
                "udpPort": 2344,
                "sendIndexes": [0, 1],
                "ssrcs": [123, 456],
                "constraints": {
                    "audio": {"maxSampleRate": 44100, "maxChannels": 2}
                }
            }"#,
        )
        .unwrap();
        let configuration = VideoTransportConfiguration {
            width: 640,
            height: 480,
            framerate_numerator: 60,
            framerate_denominator: 1,
            bitrate: 2_000_000,
            target_playout_delay: Duration::from_millis(33),
            audio: Some(AudioTransportConfiguration {
                sample_rate: 48_000,
                channels: 2,
                bitrate: 128_000,
            }),
        };
        assert!(validate_answer_constraints(&answer, configuration).is_err());
    }

    #[test]
    fn shared_delay_ceiling_uses_the_tighter_stream_constraint() {
        let answer: Answer = serde_json::from_str(
            r#"{
                "udpPort": 2344,
                "sendIndexes": [0, 1],
                "ssrcs": [123, 456],
                "constraints": {
                    "audio": {"maxDelay": 120},
                    "video": {"maxDelay": 250}
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            maximum_playout_delay(&answer, true),
            Some(Duration::from_millis(120))
        );
        assert_eq!(
            maximum_playout_delay(&answer, false),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn sender_events_project_acknowledgement_and_terminal_failure() {
        let (terminal_tx, terminal) = tokio::sync::watch::channel(None);
        let (feedback_tx, feedback) =
            tokio::sync::watch::channel(VideoTransportFeedbackSnapshot::default());
        assert!(project_sender_event(
            SenderEvent::FrameAcked {
                stream: StreamType::Video,
                frame_id: FrameId::first(),
            },
            &terminal_tx,
            &feedback_tx,
        ));
        assert_eq!(feedback.borrow().acknowledged_frames, 1);
        assert!(project_sender_event(
            SenderEvent::FrameAcked {
                stream: StreamType::Audio,
                frame_id: FrameId::first(),
            },
            &terminal_tx,
            &feedback_tx,
        ));
        assert_eq!(feedback.borrow().acknowledged_audio_packets, 1);
        assert!(terminal.borrow().is_none());

        assert!(!project_sender_event(
            SenderEvent::ReceiverTimedOut,
            &terminal_tx,
            &feedback_tx,
        ));
        let expected = VideoTransportError::new("Cast receiver stopped acknowledging media");
        assert_eq!(terminal.borrow().as_ref(), Some(&expected));
        assert_eq!(feedback.borrow().terminal_error.as_ref(), Some(&expected));
    }

    #[test]
    fn audio_and_video_must_confirm_the_same_playout_delay() {
        let (terminal, _terminal_rx) = tokio::sync::watch::channel(None);
        let (feedback, projected) =
            tokio::sync::watch::channel(VideoTransportFeedbackSnapshot::default());
        let mut audio = StreamStatistics::default();
        audio.receiver_playout_delay = Some(Duration::from_millis(33));
        let mut video = StreamStatistics::default();
        video.receiver_playout_delay = Some(Duration::from_millis(66));
        let mut statistics = SessionStatistics::default();
        statistics.audio = Some(audio);
        statistics.video = Some(video.clone());

        assert!(project_sender_event(
            SenderEvent::StatisticsUpdated(Box::new(statistics)),
            &terminal,
            &feedback,
        ));
        assert_eq!(
            projected.borrow().pressure.unwrap().receiver_playout_delay,
            None
        );

        let mut statistics = SessionStatistics::default();
        statistics.audio = Some(video.clone());
        statistics.video = Some(video);
        assert!(project_sender_event(
            SenderEvent::StatisticsUpdated(Box::new(statistics)),
            &terminal,
            &feedback,
        ));
        assert_eq!(
            projected.borrow().pressure.unwrap().receiver_playout_delay,
            Some(Duration::from_millis(66))
        );
    }

    #[tokio::test]
    async fn sender_event_task_shutdown_is_bounded() {
        let task = tokio::spawn(std::future::pending());
        let error = join_sender_event_task(task, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("timed out stopping Cast sender event task"));
    }
}
