use async_trait::async_trait;
use std::os::fd::OwnedFd as StdOwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pronk_backend_protocol::{
    AudioProfile, DisplayIdentity, DisplayMode, IdentitySource, MediaKind, RenderDeviceIdentity,
    VideoProfile, SESSION_FEATURE_AUDIO,
};
use pronk_media::{MediaGraphConfiguration, VideoFrameDependency, OPUS_BITRATE, OPUS_SAMPLE_RATE};

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
        self.statistics.video_cadence_denominator = configuration.video_cadence.denominator.get();
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

    async fn request_key_frame(&mut self, generation: NonZeroU64) -> Result<(), MediaGraphError> {
        assert_eq!(self.generation, Some(generation));
        self.statistics.key_frame_requests = self.statistics.key_frame_requests.saturating_add(1);
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
    assert_eq!(media.state(), SessionState::Prepared);
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
        assert_eq!(media.state(), SessionState::Prepared);
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
    media
        .configure(
            remote(),
            vec![target(session_id, 2)],
            configuration(),
            2,
            &mut transport,
        )
        .await
        .unwrap();
    assert_eq!(media.state(), SessionState::Configured);
    media.stop_media(2, &mut transport).await.unwrap();
    assert_eq!(transport.stops, 2);
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
        let mut media =
            ChromiacastMediaSession::with_graph(session_id.into(), 7, Box::new(graph), receiver);
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
