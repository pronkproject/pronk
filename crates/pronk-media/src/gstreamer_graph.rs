use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::BaseSrcExt;
use gstreamer_video as gst_video;
use nix::errno::Errno;
use nix::sys::socket::{getpeername, getsockopt, recv, sockopt, MsgFlags, SockType, UnixAddr};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::encoded_output::{EncodedAudioOutput, EncodedVideoOutput, OutputAdmission};
use crate::gstreamer_audio::{GStreamerAudioBranch, RawEncodedAudioPacket};
use crate::h264;
use crate::media_timeline::{GenerationMediaTimeline, MediaStreamKind};
use crate::model::{
    parse_caps, EncodedAudioPacket, EncodedVideoAccessUnit, MediaGraphConfiguration,
    MediaGraphError, MediaGraphStatistics, VideoCodec, VideoFrameDependency, OPUS_BITRATE,
    OPUS_CHANNELS, OPUS_SAMPLE_RATE, VIDEO_FRAME_RATE,
};
use crate::vp8;

// These remain strictly inside the backend protocol's 5-second stop and
// 15-second media-control deadlines, leaving time for D-Bus dispatch and
// generation cleanup around the graph operation.
const PIPELINE_STATE_TIMEOUT: Duration = Duration::from_secs(3);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_QUANTUM: Duration = Duration::from_millis(20);
const RAW_QUEUE_BUFFERS: u32 = 2;
const VIDEO_FRAME_INTERVAL_NANOSECONDS: u64 = 1_000_000_000 / VIDEO_FRAME_RATE as u64;
const VIDEO_FRAME_INTERVAL_TOLERANCE_NANOSECONDS: u64 = 2_000_000;

impl VideoCodec {
    fn name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::H264 => "H.264",
        }
    }

    fn encoder_name(self) -> &'static str {
        match self {
            Self::Vp8 => vp8::ENCODER_NAME,
            Self::H264 => h264::ENCODER_NAME,
        }
    }

    fn encoder_input_caps(self) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Vp8 => vp8::encoder_input_caps(),
            Self::H264 => h264::encoder_input_caps(),
        }
    }

    fn encoder_output_caps(self) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Vp8 => vp8::encoder_output_caps(),
            Self::H264 => h264::encoder_output_caps(),
        }
    }

    fn build_encoder(self, bitrate: NonZeroU64) -> Result<gst::Element, MediaGraphError> {
        match self {
            Self::Vp8 => gst::ElementFactory::make(vp8::ENCODER_NAME)
                .name("pronk-vp8-encoder")
                .property("deadline", 1_i64)
                .property("cpu-used", 8_i32)
                .property_from_str("end-usage", "cbr")
                .property("undershoot", 100_i32)
                .property("overshoot", 15_i32)
                .property("buffer-initial-size", 500_i32)
                .property("buffer-optimal-size", 600_i32)
                .property("buffer-size", 1_000_i32)
                .property("target-bitrate", vp8::bitrate(bitrate.get())?)
                .property_from_str("keyframe-mode", "disabled")
                .property("keyframe-max-dist", vp8::key_frame_interval())
                .property("lag-in-frames", 0_i32)
                .property("threads", 8_i32)
                .property("static-threshold", 100_i32)
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct {}: {error}", vp8::ENCODER_NAME))
                }),
            Self::H264 => gst::ElementFactory::make(h264::ENCODER_NAME)
                .name("pronk-h264-encoder")
                .property_from_str("tune", "zerolatency")
                .property_from_str("speed-preset", "ultrafast")
                .property("bitrate", h264::bitrate_kbits(bitrate.get())?)
                .property("key-int-max", h264::key_frame_interval())
                .property("bframes", 0_u32)
                .property("byte-stream", true)
                .property("aud", true)
                .property("sliced-threads", true)
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct {}: {error}", h264::ENCODER_NAME))
                }),
        }
    }

    fn build_parser(self) -> Result<Option<gst::Element>, MediaGraphError> {
        match self {
            Self::Vp8 => Ok(None),
            Self::H264 => gst::ElementFactory::make("h264parse")
                .name("pronk-h264-parser")
                .property("config-interval", -1_i32)
                .property("disable-passthrough", true)
                .build()
                .map(Some)
                .map_err(|error| MediaGraphError::new(format!("construct H.264 parser: {error}"))),
        }
    }

    fn effective_bitrate(self, bitrate: NonZeroU64) -> Result<u64, MediaGraphError> {
        match self {
            Self::Vp8 => u64::try_from(vp8::bitrate(bitrate.get())?)
                .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative")),
            Self::H264 => Ok(u64::from(h264::bitrate_kbits(bitrate.get())?).saturating_mul(1_000)),
        }
    }

    fn set_bitrate(
        self,
        encoder: &gst::Element,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        match self {
            Self::Vp8 => {
                let bitrate = vp8::bitrate(bitrate.get())?;
                encoder.set_property("target-bitrate", bitrate);
                u64::try_from(bitrate)
                    .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative"))
            }
            Self::H264 => {
                let bitrate = h264::bitrate_kbits(bitrate.get())?;
                encoder.set_property("bitrate", bitrate);
                Ok(u64::from(bitrate).saturating_mul(1_000))
            }
        }
    }

    fn validate_caps(self, caps: &gst::CapsRef) -> Result<(), MediaGraphError> {
        match self {
            Self::Vp8 => vp8::validate_caps(caps),
            Self::H264 => h264::validate_caps(caps),
        }
    }

    fn validate_frame(
        self,
        bytes: &[u8],
        dependency: VideoFrameDependency,
        first: bool,
    ) -> Result<(), MediaGraphError> {
        match self {
            Self::Vp8 => vp8::validate_frame(bytes, dependency, first),
            Self::H264 => h264::validate_access_unit(bytes, dependency, first),
        }
    }
}

fn video_stream_properties() -> gst::Structure {
    gst::Structure::builder("pronk-pipewire-stream")
        .field("node.dont-fallback", "true")
        .field("node.dont-reconnect", "true")
        // GstBaseSrc waits against a buffer's presentation timestamp while a
        // live source still owns the dequeued PipeWire buffer. The pipeline
        // already uses the system clock explicitly, so make this stream
        // non-live and return shared CastKMS buffers immediately after
        // conversion.
        .field("stream.is-live", "false")
        .build()
}

fn is_segment_anchor(buffer: &gst::BufferRef) -> bool {
    buffer.pts().is_some() && !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
}

fn install_video_frame_rate_gate(rate: &gst::Element) -> Result<(), MediaGraphError> {
    let source_pad = rate
        .static_pad("src")
        .ok_or_else(|| MediaGraphError::new("videorate has no static source pad"))?;
    let last_forwarded_timestamp = Arc::new(Mutex::new(None::<u64>));
    source_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(timestamp) = info
                .buffer()
                .and_then(|buffer| buffer.pts())
                .map(|timestamp| timestamp.nseconds())
            else {
                return gst::PadProbeReturn::Ok;
            };
            let mut last = last_forwarded_timestamp
                .lock()
                .expect("video frame-rate gate mutex poisoned");
            let minimum_interval = VIDEO_FRAME_INTERVAL_NANOSECONDS
                .saturating_sub(VIDEO_FRAME_INTERVAL_TOLERANCE_NANOSECONDS);
            if last.is_some_and(|previous| {
                timestamp >= previous && timestamp - previous < minimum_interval
            }) {
                return gst::PadProbeReturn::Drop;
            }
            *last = Some(timestamp);
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| MediaGraphError::new("install video frame-rate gate"))?;
    Ok(())
}

pub(crate) struct GStreamerGraph {
    generation: NonZeroU64,
    video_codec: VideoCodec,
    pipeline: gst::Pipeline,
    video_source: gst_base::BaseSrc,
    encoder: gst::Element,
    app_sink: gst_app::AppSink,
    expected_encoded_caps: gst::Caps,
    effective_encoded_caps: Option<String>,
    _video_remote: std::os::fd::OwnedFd,
    audio: Option<GStreamerAudioBranch>,
    statistics: MediaGraphStatistics,
    video_output: Option<EncodedVideoOutput>,
    audio_output: Option<EncodedAudioOutput>,
    timeline: Option<GenerationMediaTimeline>,
    timeline_needs_reanchor: bool,
    needs_segment_key_frame: bool,
    pending_video: Option<gst::Sample>,
    pending_audio: Option<RawEncodedAudioPacket>,
}

impl std::fmt::Debug for GStreamerGraph {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GStreamerGraph")
            .field("generation", &self.generation)
            .field("video_codec", &self.video_codec)
            .field("statistics", &self.statistics)
            .finish_non_exhaustive()
    }
}

impl GStreamerGraph {
    pub(crate) fn configure(
        configuration: MediaGraphConfiguration,
        video_output: Option<mpsc::Sender<EncodedVideoAccessUnit>>,
        audio_output: Option<mpsc::Sender<EncodedAudioPacket>>,
    ) -> Result<Self, MediaGraphError> {
        validate_remote_socket(configuration.video.remote.as_fd())?;
        gst::init().map_err(|error| {
            MediaGraphError::new(format!("initialize backend GStreamer: {error}"))
        })?;
        if configuration.video.node_name.is_empty() {
            return Err(MediaGraphError::new("PipeWire video node name is empty"));
        }
        let (raw_caps, _) = parse_caps(&configuration.video.caps)?;
        let video_codec = configuration.video_codec;
        let converted_caps = video_codec.encoder_input_caps()?;
        let encoded_caps = video_codec.encoder_output_caps()?;
        let stream_properties = video_stream_properties();
        let source = gst::ElementFactory::make("pipewiresrc")
            .name("pronk-video-source")
            .property("fd", configuration.video.remote.as_raw_fd())
            // Pronk node names are unique and never reused across media
            // generations. WirePlumber 0.5.15 resolves those names reliably,
            // while its SiLinkable lookup does not resolve PipeWire numeric
            // object serials for these virtual video sources.
            .property("target-object", &configuration.video.node_name)
            .property("stream-properties", stream_properties)
            .property("autoconnect", true)
            // Preserve the imported DMA-BUF pool. `stream.is-live=false`
            // above prevents GstBaseSrc from holding one of these buffers in
            // a live presentation timestamp wait, and the asynchronous queue
            // remains after conversion so it retains I420 allocations rather
            // than BGRx capture buffers.
            .property("use-bufferpool", true)
            .property(
                "client-name",
                format!("pronk-backend-media-{}", configuration.media_generation),
            )
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct exact-target pipewiresrc: {error}"))
            })?;
        let video_source = source
            .clone()
            .downcast::<gst_base::BaseSrc>()
            .map_err(|_| MediaGraphError::new("pipewiresrc is not a GstBaseSrc"))?;
        let caps_filter = gst::ElementFactory::make("capsfilter")
            .name("pronk-video-caps")
            .property("caps", &raw_caps)
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct video caps filter: {error}"))
            })?;
        let queue = gst::ElementFactory::make("queue")
            .name("pronk-video-encoder-queue")
            .property("max-size-buffers", RAW_QUEUE_BUFFERS)
            .property("max-size-bytes", 0_u32)
            .property("max-size-time", 0_u64)
            .property_from_str("leaky", "downstream")
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct bounded video queue: {error}"))
            })?;
        let convert = gst::ElementFactory::make("videoconvert")
            .name("pronk-video-convert")
            .build()
            .map_err(|error| MediaGraphError::new(format!("construct video converter: {error}")))?;
        let rate = gst::ElementFactory::make("videorate")
            .name("pronk-video-rate")
            .property("drop-only", true)
            .property("max-rate", VIDEO_FRAME_RATE as i32)
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct video rate limiter: {error}"))
            })?;
        install_video_frame_rate_gate(&rate)?;
        let converted_caps_filter = gst::ElementFactory::make("capsfilter")
            .name("pronk-encoder-input-caps")
            .property("caps", &converted_caps)
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct encoder input caps filter: {error}"))
            })?;
        let encoder = video_codec.build_encoder(configuration.video_bitrate)?;
        let parser = video_codec.build_parser()?;
        let encoded_caps_filter = gst::ElementFactory::make("capsfilter")
            .name("pronk-encoded-video-caps")
            .property("caps", &encoded_caps)
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!(
                    "construct {} caps filter: {error}",
                    video_codec.name()
                ))
            })?;
        let app_sink = gst::ElementFactory::make("appsink")
            .name("pronk-encoded-video-sink")
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct {} appsink: {error}", video_codec.name()))
            })?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| MediaGraphError::new("appsink factory returned another element type"))?;
        app_sink.set_caps(Some(&encoded_caps));
        app_sink.set_max_buffers(RAW_QUEUE_BUFFERS);
        app_sink.set_drop(true);
        app_sink.set_wait_on_eos(false);
        app_sink.set_sync(false);

        let worker = std::thread::current();
        wake_worker_on_output(&app_sink, worker.clone());

        let pipeline = gst::Pipeline::with_name("pronk-backend-media");
        // Video capture timestamps come from the kernel's monotonic clock,
        // while the video and audio PipeWire nodes can belong to independent
        // driver graphs. If GStreamer selects either pipewiresrc clock, a
        // generation restart can make the other source appear to be ahead and
        // pace it in buffer-pool-sized bursts. Keep both branches in the
        // timestamp domain shared by their kernel producers.
        pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        pipeline
            .add_many([
                &source,
                &caps_filter,
                &convert,
                &rate,
                &converted_caps_filter,
                &queue,
                &encoder,
                &encoded_caps_filter,
                app_sink.upcast_ref(),
            ])
            .map_err(|error| {
                MediaGraphError::new(format!("assemble backend video pipeline: {error}"))
            })?;
        if let Some(parser) = &parser {
            pipeline
                .add(parser)
                .map_err(|error| MediaGraphError::new(format!("assemble H.264 parser: {error}")))?;
        }
        gst::Element::link_many([
            &source,
            &caps_filter,
            &convert,
            &rate,
            &converted_caps_filter,
            &queue,
            &encoder,
        ])
        .map_err(|error| MediaGraphError::new(format!("link backend video pipeline: {error}")))?;
        match &parser {
            Some(parser) => gst::Element::link_many([
                &encoder,
                parser,
                &encoded_caps_filter,
                app_sink.upcast_ref(),
            ]),
            None => {
                gst::Element::link_many([&encoder, &encoded_caps_filter, app_sink.upcast_ref()])
            }
        }
        .map_err(|error| {
            MediaGraphError::new(format!(
                "link {} encoder output: {error}",
                video_codec.name()
            ))
        })?;

        let audio = configuration
            .audio
            .map(|input| {
                GStreamerAudioBranch::add_to_pipeline(
                    &pipeline,
                    input,
                    configuration.media_generation.get(),
                )
            })
            .transpose()?;
        if let Some(audio) = &audio {
            audio.wake_worker_on_output(worker.clone());
        }
        let bus = pipeline
            .bus()
            .ok_or_else(|| MediaGraphError::new("backend media pipeline has no bus"))?;
        bus.set_sync_handler(move |_, message| {
            if matches!(
                message.type_(),
                gst::MessageType::Error | gst::MessageType::Eos
            ) {
                worker.unpark();
            }
            gst::BusSyncReply::Pass
        });
        let has_audio = audio.is_some();

        let effective_bitrate = video_codec.effective_bitrate(configuration.video_bitrate)?;
        let graph = Self {
            generation: configuration.media_generation,
            video_codec,
            pipeline,
            video_source,
            encoder,
            app_sink,
            expected_encoded_caps: encoded_caps.clone(),
            effective_encoded_caps: None,
            _video_remote: configuration.video.remote,
            audio,
            statistics: MediaGraphStatistics {
                video_bitrate: effective_bitrate,
                encoder_name: Some(video_codec.encoder_name().into()),
                encoded_caps: Some(encoded_caps.to_string()),
                audio_encoder_name: has_audio.then(|| "opusenc".into()),
                encoded_audio_caps: has_audio.then(|| {
                    format!(
                        "audio/x-opus,rate={OPUS_SAMPLE_RATE},channels={OPUS_CHANNELS},channel-mapping-family=0,bitrate={OPUS_BITRATE}"
                    )
                }),
                ..MediaGraphStatistics::default()
            },
            video_output: video_output.map(EncodedVideoOutput::new),
            audio_output: has_audio
                .then(|| audio_output.map(EncodedAudioOutput::new))
                .flatten(),
            timeline: None,
            timeline_needs_reanchor: false,
            needs_segment_key_frame: true,
            pending_video: None,
            pending_audio: None,
        };
        if let Err(error) = graph.change_state(gst::State::Paused, PIPELINE_STATE_TIMEOUT) {
            let _ = graph.change_state(gst::State::Null, PIPELINE_STATE_TIMEOUT);
            return Err(error);
        }
        Ok(graph)
    }

    pub(crate) fn start(&mut self) -> Result<(), MediaGraphError> {
        let previous_video = self.statistics.frames;
        let previous_audio = self.statistics.audio_packets;
        self.change_state(gst::State::Playing, PIPELINE_STATE_TIMEOUT)?;
        self.force_key_frame()?;
        self.wait_for_media_after(previous_video, previous_audio, FIRST_FRAME_TIMEOUT)
    }

    pub(crate) fn suspend(&mut self) -> Result<(), MediaGraphError> {
        // READY is the stable quiescent state for pipewiresrc. The transition
        // interrupts its streaming task and completes the flush before a
        // subsequent activation negotiates again.
        self.change_state(gst::State::Ready, PIPELINE_STATE_TIMEOUT)?;
        self.pending_video = None;
        while self
            .app_sink
            .try_pull_sample(gst::ClockTime::ZERO)
            .is_some()
        {}
        self.pending_audio = None;
        if let Some(audio) = &mut self.audio {
            audio.begin_segment();
        }
        self.timeline_needs_reanchor = self.timeline.is_some();
        self.needs_segment_key_frame = true;
        Ok(())
    }

    pub(crate) fn resume(&mut self) -> Result<(), MediaGraphError> {
        let previous_video = self.statistics.frames;
        let previous_audio = self.statistics.audio_packets;
        self.prepare_non_live_resume()?;
        self.change_state(gst::State::Playing, PIPELINE_STATE_TIMEOUT)?;
        self.force_key_frame()?;
        self.wait_for_media_after(previous_video, previous_audio, FIRST_FRAME_TIMEOUT)
    }

    fn prepare_non_live_resume(&self) -> Result<(), MediaGraphError> {
        // pipewiresrc applies stream.is-live while connecting, after
        // GstBaseSrc has already classified the READY-to-PAUSED transition.
        // Restore its construction-time classification for that transition,
        // then make it non-live again before any resumed frame is delivered.
        self.video_source.set_live(true);
        let result = self.change_state(gst::State::Paused, PIPELINE_STATE_TIMEOUT);
        self.video_source.set_live(false);
        result
    }

    pub(crate) fn poll(&mut self, wait: Duration) -> Result<(), MediaGraphError> {
        self.poll_bus(wait)?;
        self.pull_available_samples()
    }

    pub(crate) fn statistics(&self) -> MediaGraphStatistics {
        self.statistics.clone()
    }

    pub(crate) fn request_key_frame(&mut self) -> Result<(), MediaGraphError> {
        self.force_key_frame()?;
        self.statistics.key_frame_requests = self.statistics.key_frame_requests.saturating_add(1);
        Ok(())
    }

    fn force_key_frame(&self) -> Result<(), MediaGraphError> {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if !self.encoder.send_event(event) {
            return Err(MediaGraphError::new(format!(
                "{} rejected an upstream force-key-unit event",
                self.video_codec.encoder_name()
            )));
        }
        Ok(())
    }

    pub(crate) fn set_video_bitrate(
        &mut self,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        let effective_bitrate = self.video_codec.set_bitrate(&self.encoder, bitrate)?;
        if self.statistics.video_bitrate != effective_bitrate {
            self.statistics.video_bitrate = effective_bitrate;
            self.statistics.bitrate_changes = self.statistics.bitrate_changes.saturating_add(1);
        }
        self.poll_bus(Duration::ZERO)?;
        Ok(effective_bitrate)
    }

    pub(crate) fn stop(mut self) -> Result<MediaGraphStatistics, MediaGraphError> {
        self.pull_available_samples()?;
        let result = self.change_state(gst::State::Null, PIPELINE_STATE_TIMEOUT);
        let statistics = self.statistics.clone();
        result.map(|()| statistics)
    }

    fn wait_for_media_after(
        &mut self,
        previous_video: u64,
        previous_audio: u64,
        wait: Duration,
    ) -> Result<(), MediaGraphError> {
        let deadline = Instant::now() + wait;
        while self.statistics.frames <= previous_video
            || (self.audio.is_some() && self.statistics.audio_packets <= previous_audio)
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(MediaGraphError::new(format!(
                    "timed out after {wait:?} waiting for generation-{} encoded media",
                    self.generation
                )));
            }
            self.poll(remaining.min(POLL_QUANTUM))?;
        }
        Ok(())
    }

    fn change_state(&self, desired: gst::State, wait: Duration) -> Result<(), MediaGraphError> {
        if let Err(state_error) = self.pipeline.set_state(desired) {
            let mut element_states = self
                .pipeline
                .iterate_elements()
                .into_iter()
                .filter_map(Result::ok)
                .map(|element| {
                    let (result, current, pending) = element.state(Some(gst::ClockTime::ZERO));
                    format!("{}={current:?}/{pending:?} ({result:?})", element.name())
                })
                .collect::<Vec<_>>();
            // GStreamer iterates bins downstream-first. Put the source first,
            // where live-pipeline state failures most commonly originate.
            element_states.reverse();
            let element_states = element_states.join(", ");
            // Some elements post their detailed bus error immediately after
            // returning the generic state-change failure. Give that message
            // one bounded dispatch quantum so callers receive the real cause.
            if let Err(pipeline_error) = self.poll_bus(POLL_QUANTUM) {
                return Err(MediaGraphError::new(format!(
                    "set backend media pipeline to {desired:?}: {state_error}; {pipeline_error}; element states: {element_states}"
                )));
            }
            return Err(MediaGraphError::new(format!(
                "set backend media pipeline to {desired:?}: {state_error}; element states: {element_states}"
            )));
        }
        let (result, current, pending) = self
            .pipeline
            .state(Some(gst::ClockTime::from_nseconds(duration_ns(wait))));
        result.map_err(|error| {
            MediaGraphError::new(format!(
                "wait for backend media pipeline state {desired:?}: {error}"
            ))
        })?;
        if current != desired {
            return Err(MediaGraphError::new(format!(
                "backend media pipeline reached {current:?} with {pending:?} pending instead of {desired:?}"
            )));
        }
        self.poll_bus(Duration::ZERO)
    }

    fn poll_bus(&self, wait: Duration) -> Result<(), MediaGraphError> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| MediaGraphError::new("backend media pipeline has no bus"))?;
        let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(duration_ns(wait)),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        ) else {
            return Ok(());
        };
        match message.view() {
            gst::MessageView::Error(error) => {
                let source = error
                    .src()
                    .map(|source| source.path_string().to_string())
                    .unwrap_or_else(|| "unknown GStreamer object".into());
                Err(MediaGraphError::new(format!(
                    "backend media pipeline error from {source}: {} ({})",
                    error.error(),
                    error.debug().unwrap_or_else(|| "no debug detail".into())
                )))
            }
            gst::MessageView::Eos(_) => Err(MediaGraphError::new(
                "backend media pipeline reached unexpected end-of-stream",
            )),
            _ => unreachable!("bus filter returned an unrequested message"),
        }
    }

    fn pull_available_samples(&mut self) -> Result<(), MediaGraphError> {
        if self.timeline.is_none() || self.timeline_needs_reanchor {
            if self.pending_video.is_none() {
                self.pending_video = self.pull_timeline_anchor_sample()?;
            }
            if self.pending_audio.is_none() {
                if let Some(audio) = self.audio.as_mut() {
                    self.pending_audio = audio.try_pull_packet()?;
                }
            }
            let Some(video) = self.pending_video.as_ref() else {
                return Ok(());
            };
            let video_pts = sample_pts(video, "video")?;
            let audio_pts = if self.audio.is_some() {
                let Some(audio) = self.pending_audio.as_ref() else {
                    return Ok(());
                };
                Some(audio.pts)
            } else {
                None
            };
            let reference_origin = Instant::now();
            if let Some(timeline) = &mut self.timeline {
                timeline.reanchor(video_pts, audio_pts, reference_origin)?;
            } else {
                self.timeline = Some(GenerationMediaTimeline::new(
                    video_pts,
                    audio_pts,
                    reference_origin,
                ));
            }
            self.timeline_needs_reanchor = false;
        }

        if let Some(sample) = self.pending_video.take() {
            self.consume_video_sample(&sample)?;
        }
        if let Some(packet) = self.pending_audio.take() {
            self.consume_audio_packet(packet)?;
        }
        while let Some(sample) = self.app_sink.try_pull_sample(gst::ClockTime::ZERO) {
            self.consume_video_sample(&sample)?;
        }
        loop {
            let packet = match self.audio.as_mut() {
                Some(audio) => audio.try_pull_packet()?,
                None => None,
            };
            let Some(packet) = packet else {
                break;
            };
            self.consume_audio_packet(packet)?;
        }
        Ok(())
    }

    fn pull_timeline_anchor_sample(&mut self) -> Result<Option<gst::Sample>, MediaGraphError> {
        loop {
            let Some(sample) = self.app_sink.try_pull_sample(gst::ClockTime::ZERO) else {
                return Ok(None);
            };
            if !self.needs_segment_key_frame {
                return Ok(Some(sample));
            }
            let buffer = sample
                .buffer()
                .ok_or_else(|| MediaGraphError::new("encoded video sample has no buffer"))?;
            // A newly linked PipeWire stream can expose one transition buffer
            // without a presentation timestamp. Do not let that buffer, or a
            // delta frame queued before the forced key frame, anchor the new
            // media segment, including the initial segment when another media
            // branch delays collection long enough to fill the appsink.
            if is_segment_anchor(buffer) {
                return Ok(Some(sample));
            }
        }
    }

    fn consume_video_sample(&mut self, sample: &gst::Sample) -> Result<(), MediaGraphError> {
        let caps = sample
            .caps()
            .ok_or_else(|| MediaGraphError::new("encoded video sample has no caps"))?;
        if !caps.can_intersect(&self.expected_encoded_caps) {
            return Err(MediaGraphError::new(format!(
                "encoded video sample caps {caps} differ from required {}",
                self.expected_encoded_caps
            )));
        }
        self.video_codec.validate_caps(caps)?;
        let effective_caps = caps.to_string();
        match &self.effective_encoded_caps {
            Some(previous) if previous != &effective_caps => {
                return Err(MediaGraphError::new(format!(
                    "encoded video caps changed within generation {} from {previous} to {effective_caps}",
                    self.generation
                )));
            }
            None => {
                self.statistics.encoded_caps = Some(effective_caps.clone());
                self.effective_encoded_caps = Some(effective_caps);
            }
            Some(_) => {}
        }
        let buffer = sample
            .buffer()
            .ok_or_else(|| MediaGraphError::new("encoded video sample has no buffer"))?;
        let pts = buffer.pts().map(|pts| pts.nseconds()).ok_or_else(|| {
            MediaGraphError::new("encoded video access unit has no presentation timestamp")
        })?;
        if buffer.dts().is_some_and(|dts| dts.nseconds() != pts) {
            return Err(MediaGraphError::new(format!(
                "zero-reorder {} frame has decoding timestamp {:?} but presentation timestamp {pts}",
                self.video_codec.name(),
                buffer.dts().map(|dts| dts.nseconds())
            )));
        }
        let duration_nanos = buffer
            .duration()
            .map(|duration| duration.nseconds())
            .filter(|duration| *duration > 0)
            .ok_or_else(|| MediaGraphError::new("encoded video access unit has no duration"))?;
        let mapped = buffer.map_readable().map_err(|error| {
            MediaGraphError::new(format!("map encoded video access unit read-only: {error}"))
        })?;
        let bytes = mapped.as_slice();
        let dependency = if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            VideoFrameDependency::Delta
        } else {
            VideoFrameDependency::KeyFrame
        };
        self.video_codec
            .validate_frame(bytes, dependency, self.needs_segment_key_frame)?;

        let (media_timestamp, reference_time) = self
            .timeline
            .as_mut()
            .ok_or_else(|| MediaGraphError::new("generation media timeline is absent"))?
            .timing(
                MediaStreamKind::Video,
                pts,
                Duration::from_nanos(duration_nanos),
            )?;
        let access_unit = EncodedVideoAccessUnit {
            media_generation: self.generation,
            dependency,
            data: bytes.to_vec(),
            media_timestamp,
            reference_time,
            duration: Duration::from_nanos(duration_nanos),
        };

        let mut digest = Sha256::new();
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
        self.statistics.frames = self.statistics.frames.saturating_add(1);
        self.statistics.key_frames = self
            .statistics
            .key_frames
            .saturating_add(u64::from(dependency == VideoFrameDependency::KeyFrame));
        self.statistics.encoded_bytes = self
            .statistics
            .encoded_bytes
            .saturating_add(bytes.len() as u64);
        self.statistics.bytes_hashed = self
            .statistics
            .bytes_hashed
            .saturating_add(bytes.len() as u64);
        self.statistics.last_frame_hash = Some(digest.finalize().into());
        self.statistics.first_pts_nanos.get_or_insert(pts);
        self.statistics.last_pts_nanos = Some(pts);
        self.needs_segment_key_frame = false;
        self.forward_access_unit(access_unit)
    }

    fn consume_audio_packet(
        &mut self,
        packet: RawEncodedAudioPacket,
    ) -> Result<(), MediaGraphError> {
        let (media_timestamp, reference_time) = self
            .timeline
            .as_mut()
            .ok_or_else(|| MediaGraphError::new("generation media timeline is absent"))?
            .timing(MediaStreamKind::Audio, packet.pts, packet.duration)?;
        let encoded_bytes = packet.data.len() as u64;
        let encoded = EncodedAudioPacket {
            media_generation: self.generation,
            data: packet.data,
            media_timestamp,
            reference_time,
            duration: packet.duration,
        };
        self.statistics.audio_packets = self.statistics.audio_packets.saturating_add(1);
        self.statistics.encoded_audio_bytes = self
            .statistics
            .encoded_audio_bytes
            .saturating_add(encoded_bytes);
        self.statistics
            .first_audio_pts_nanos
            .get_or_insert(packet.pts);
        self.statistics.last_audio_pts_nanos = Some(packet.pts);
        self.statistics.encoded_audio_caps = Some(packet.caps);
        self.forward_audio_packet(encoded)
    }

    fn forward_access_unit(
        &mut self,
        access_unit: EncodedVideoAccessUnit,
    ) -> Result<(), MediaGraphError> {
        let Some(output) = &mut self.video_output else {
            return Ok(());
        };
        match output.try_forward(access_unit)? {
            OutputAdmission::Forwarded => Ok(()),
            OutputAdmission::Dropped { request_key_frame } => {
                self.statistics.dropped_frames = self.statistics.dropped_frames.saturating_add(1);
                if request_key_frame {
                    self.request_key_frame()?;
                }
                Ok(())
            }
        }
    }

    fn forward_audio_packet(&mut self, packet: EncodedAudioPacket) -> Result<(), MediaGraphError> {
        let Some(output) = &mut self.audio_output else {
            return Ok(());
        };
        match output.try_forward(packet)? {
            OutputAdmission::Forwarded => Ok(()),
            OutputAdmission::Dropped { .. } => {
                self.statistics.dropped_audio_packets =
                    self.statistics.dropped_audio_packets.saturating_add(1);
                Ok(())
            }
        }
    }
}

fn wake_worker_on_output(app_sink: &gst_app::AppSink, worker: std::thread::Thread) {
    let preroll_worker = worker.clone();
    let sample_worker = worker.clone();
    app_sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_preroll(move |_| {
                preroll_worker.unpark();
                Ok(gst::FlowSuccess::Ok)
            })
            .new_sample(move |_| {
                sample_worker.unpark();
                Ok(gst::FlowSuccess::Ok)
            })
            .eos(move |_| worker.unpark())
            .build(),
    );
}

impl Drop for GStreamerGraph {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn sample_pts(sample: &gst::Sample, label: &str) -> Result<u64, MediaGraphError> {
    sample
        .buffer()
        .and_then(|buffer| buffer.pts())
        .map(|pts| pts.nseconds())
        .ok_or_else(|| MediaGraphError::new(format!("encoded {label} sample has no timestamp")))
}

pub(crate) fn validate_remote_socket(fd: BorrowedFd<'_>) -> Result<(), MediaGraphError> {
    let socket_type = getsockopt(&fd, sockopt::SockType).map_err(|error| {
        MediaGraphError::new(format!(
            "inspect passed PipeWire remote socket type: {error}"
        ))
    })?;
    if socket_type != SockType::Stream {
        return Err(MediaGraphError::new(format!(
            "passed PipeWire remote is {socket_type:?}, not a stream socket"
        )));
    }
    getpeername::<UnixAddr>(fd.as_raw_fd()).map_err(|error| {
        MediaGraphError::new(format!(
            "passed PipeWire remote is not a connected Unix socket: {error}"
        ))
    })?;

    let mut byte = [0_u8; 1];
    match recv(
        fd.as_raw_fd(),
        &mut byte,
        MsgFlags::MSG_PEEK | MsgFlags::MSG_DONTWAIT,
    ) {
        Ok(0) => Err(MediaGraphError::new(
            "passed PipeWire remote is already disconnected",
        )),
        Ok(_) | Err(Errno::EAGAIN) => Ok(()),
        Err(error) => Err(MediaGraphError::new(format!(
            "probe passed PipeWire remote without consuming bytes: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::{UnixDatagram, UnixStream};

    use super::{is_segment_anchor, validate_remote_socket, video_stream_properties};

    #[test]
    fn video_stream_is_non_live_while_using_the_pipeline_system_clock() {
        gstreamer::init().unwrap();
        let properties = video_stream_properties();
        assert_eq!(properties.get::<String>("stream.is-live").unwrap(), "false");
    }

    #[test]
    fn segment_anchor_requires_a_timestamped_key_frame() {
        gstreamer::init().unwrap();

        let missing_timestamp = gstreamer::Buffer::new();
        assert!(!is_segment_anchor(&missing_timestamp));

        let mut delta_frame = gstreamer::Buffer::new();
        {
            let buffer = delta_frame.get_mut().unwrap();
            buffer.set_pts(gstreamer::ClockTime::ZERO);
            buffer.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
        }
        assert!(!is_segment_anchor(&delta_frame));

        let mut key_frame = gstreamer::Buffer::new();
        key_frame
            .get_mut()
            .unwrap()
            .set_pts(gstreamer::ClockTime::ZERO);
        assert!(is_segment_anchor(&key_frame));
    }

    #[test]
    fn remote_preflight_is_nonconsuming_and_rejects_closed_or_wrong_sockets() {
        let (remote, peer) = UnixStream::pair().unwrap();
        validate_remote_socket(remote.as_fd()).unwrap();
        drop(peer);
        assert!(validate_remote_socket(remote.as_fd()).is_err());

        let (datagram, _peer) = UnixDatagram::pair().unwrap();
        assert!(validate_remote_socket(datagram.as_fd()).is_err());
    }
}
