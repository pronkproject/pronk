//! Connector-bound private PipeWire source capture and Opus encoding branch.

use std::os::fd::{AsFd, AsRawFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::gstreamer_graph::validate_remote_socket;
use crate::model::{
    parse_audio_caps, MediaGraphError, PipeWireAudioInput, MAX_ENCODED_AUDIO_PACKET_BYTES,
    OPUS_BITRATE, OPUS_CHANNELS, OPUS_FRAME_DURATION, OPUS_SAMPLE_RATE,
};

const RAW_AUDIO_QUEUE_BUFFERS: u32 = 8;
const RAW_AUDIO_FRAME_BYTES: usize = OPUS_CHANNELS as usize * std::mem::size_of::<i16>();
// opusenc reports its restricted-low-delay lookahead as clipping on the first
// encoded packet. The packet still contains one configured 20 ms Opus frame,
// but its effective GStreamer timeline duration is shorter by 2.5 ms.
const OPUS_PRIMED_FRAME_DURATION: Duration = Duration::from_micros(17_500);

#[derive(Debug, Default)]
struct RawAudioTimeline {
    origin_ns: Option<u64>,
    published_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawAudioTiming {
    presentation_timestamp_ns: u64,
    duration_ns: u64,
    first_frame: u64,
    end_frame: u64,
    discontinuity: bool,
}

impl RawAudioTimeline {
    fn next(
        &mut self,
        source_presentation_timestamp_ns: Option<u64>,
        bytes: usize,
    ) -> Result<RawAudioTiming, MediaGraphError> {
        if bytes == 0 || bytes % RAW_AUDIO_FRAME_BYTES != 0 {
            return Err(MediaGraphError::new(format!(
                "raw audio buffer size {bytes} is not a positive whole number of stereo S16LE frames"
            )));
        }
        let frames = u64::try_from(bytes / RAW_AUDIO_FRAME_BYTES)
            .map_err(|_| MediaGraphError::new("raw audio frame count does not fit u64"))?;
        let discontinuity = self.origin_ns.is_none();
        let origin_ns = match self.origin_ns {
            Some(origin_ns) => origin_ns,
            None => {
                let origin_ns = source_presentation_timestamp_ns.ok_or_else(|| {
                    MediaGraphError::new("the first raw audio buffer has no presentation timestamp")
                })?;
                self.origin_ns = Some(origin_ns);
                origin_ns
            }
        };
        let first_frame = self.published_frames;
        let end_frame = first_frame
            .checked_add(frames)
            .ok_or_else(|| MediaGraphError::new("raw audio frame sequence overflowed"))?;
        let start_offset_ns = audio_frames_to_nanoseconds(first_frame)?;
        let end_offset_ns = audio_frames_to_nanoseconds(end_frame)?;
        let presentation_timestamp_ns = origin_ns
            .checked_add(start_offset_ns)
            .ok_or_else(|| MediaGraphError::new("raw audio presentation timestamp overflowed"))?;
        self.published_frames = end_frame;
        Ok(RawAudioTiming {
            presentation_timestamp_ns,
            duration_ns: end_offset_ns - start_offset_ns,
            first_frame,
            end_frame,
            discontinuity,
        })
    }
}

#[derive(Default)]
struct RawAudioTimestampState {
    timeline: RawAudioTimeline,
    failure: Option<MediaGraphError>,
    worker: Option<std::thread::Thread>,
}

fn audio_frames_to_nanoseconds(frames: u64) -> Result<u64, MediaGraphError> {
    frames
        .checked_mul(1_000_000_000)
        .and_then(|nanoseconds| nanoseconds.checked_div(u64::from(OPUS_SAMPLE_RATE)))
        .ok_or_else(|| MediaGraphError::new("raw audio presentation timestamp overflowed"))
}

fn audio_stream_properties() -> gst::Structure {
    gst::Structure::builder("pronk-pipewire-audio-stream")
        .field("node.dont-fallback", "true")
        .field("node.dont-reconnect", "true")
        // The exact target is Pronk's private Audio/Source, published from the
        // grant-scoped kernel tap. `stream.capture.sink=true` would instead
        // make PipeWire look for an Audio/Sink monitor with this name.
        .build()
}

fn install_timestamp_normalizer(
    source: &gst::Element,
    timestamp_state: Arc<Mutex<RawAudioTimestampState>>,
) -> Result<(), MediaGraphError> {
    let source_pad = source
        .static_pad("src")
        .ok_or_else(|| MediaGraphError::new("pipewiresrc has no static source pad"))?;
    source_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buffer) = info.buffer_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let source_presentation_timestamp_ns =
                buffer.pts().map(|timestamp| timestamp.nseconds());
            let bytes = buffer.size();
            let mut state = timestamp_state
                .lock()
                .expect("raw audio timestamp mutex poisoned");
            if state.failure.is_some() {
                return gst::PadProbeReturn::Drop;
            }
            let timing = match state.timeline.next(source_presentation_timestamp_ns, bytes) {
                Ok(timing) => timing,
                Err(error) => {
                    state.failure = Some(error);
                    if let Some(worker) = &state.worker {
                        worker.unpark();
                    }
                    return gst::PadProbeReturn::Drop;
                }
            };
            drop(state);

            let buffer = buffer.make_mut();
            let presentation_timestamp =
                gst::ClockTime::from_nseconds(timing.presentation_timestamp_ns);
            buffer.set_pts(presentation_timestamp);
            buffer.set_dts(presentation_timestamp);
            buffer.set_duration(gst::ClockTime::from_nseconds(timing.duration_ns));
            buffer.set_offset(timing.first_frame);
            buffer.set_offset_end(timing.end_frame);
            if timing.discontinuity {
                buffer.set_flags(gst::BufferFlags::DISCONT);
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| MediaGraphError::new("install raw audio timestamp normalizer"))?;
    Ok(())
}

pub(crate) struct RawEncodedAudioPacket {
    pub data: Vec<u8>,
    pub pts: u64,
    pub duration: Duration,
    pub caps: String,
}

pub(crate) struct GStreamerAudioBranch {
    app_sink: gst_app::AppSink,
    expected_encoded_caps: gst::Caps,
    effective_encoded_caps: Option<String>,
    encoded_packets: u64,
    timestamp_state: Arc<Mutex<RawAudioTimestampState>>,
    _remote: std::os::fd::OwnedFd,
}

impl GStreamerAudioBranch {
    pub(crate) fn add_to_pipeline(
        pipeline: &gst::Pipeline,
        input: PipeWireAudioInput,
        media_generation: u64,
    ) -> Result<Self, MediaGraphError> {
        validate_remote_socket(input.remote.as_fd())?;
        if input.node_name.is_empty() {
            return Err(MediaGraphError::new("PipeWire audio node name is empty"));
        }
        let (raw_caps, _) = parse_audio_caps(&input.caps)?;
        let encoded_caps = gst::Caps::builder("audio/x-opus")
            .field("rate", OPUS_SAMPLE_RATE as i32)
            .field("channels", OPUS_CHANNELS as i32)
            .field("channel-mapping-family", 0_i32)
            .build();
        let stream_properties = audio_stream_properties();
        let source = gst::ElementFactory::make("pipewiresrc")
            .name("pronk-audio-source")
            .property("fd", input.remote.as_raw_fd())
            .property("target-object", &input.node_name)
            .property("stream-properties", stream_properties)
            .property("autoconnect", true)
            .property("use-bufferpool", false)
            .property(
                "client-name",
                format!("pronk-backend-audio-{media_generation}"),
            )
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct exact-target audio pipewiresrc: {error}"))
            })?;
        let timestamp_state = Arc::new(Mutex::new(RawAudioTimestampState::default()));
        install_timestamp_normalizer(&source, timestamp_state.clone())?;
        let queue = gst::ElementFactory::make("queue")
            .name("pronk-audio-encoder-queue")
            .property("max-size-buffers", RAW_AUDIO_QUEUE_BUFFERS)
            .property("max-size-bytes", 0_u32)
            .property("max-size-time", 0_u64)
            .property_from_str("leaky", "downstream")
            .build()
            .map_err(|error| MediaGraphError::new(format!("construct audio queue: {error}")))?;
        let convert = gst::ElementFactory::make("audioconvert")
            .name("pronk-audio-convert")
            .build()
            .map_err(|error| MediaGraphError::new(format!("construct audio converter: {error}")))?;
        let resample = gst::ElementFactory::make("audioresample")
            .name("pronk-audio-resample")
            .build()
            .map_err(|error| MediaGraphError::new(format!("construct audio resampler: {error}")))?;
        let raw_caps_filter = gst::ElementFactory::make("capsfilter")
            .name("pronk-opus-input-caps")
            .property("caps", &raw_caps)
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct Opus input caps filter: {error}"))
            })?;
        let encoder = gst::ElementFactory::make("opusenc")
            .name("pronk-opus-encoder")
            .property("bitrate", OPUS_BITRATE as i32)
            .property_from_str("bitrate-type", "constrained-vbr")
            .property_from_str("audio-type", "restricted-lowdelay")
            .property_from_str("frame-size", "20")
            .property("perfect-timestamp", true)
            .property("inband-fec", true)
            .property("packet-loss-percentage", 5_i32)
            .build()
            .map_err(|error| MediaGraphError::new(format!("construct Opus encoder: {error}")))?;
        let encoded_caps_filter = gst::ElementFactory::make("capsfilter")
            .name("pronk-opus-caps")
            .property("caps", &encoded_caps)
            .build()
            .map_err(|error| {
                MediaGraphError::new(format!("construct Opus caps filter: {error}"))
            })?;
        let app_sink = gst::ElementFactory::make("appsink")
            .name("pronk-opus-packet-sink")
            .build()
            .map_err(|error| MediaGraphError::new(format!("construct Opus appsink: {error}")))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| MediaGraphError::new("appsink factory returned another element type"))?;
        app_sink.set_caps(Some(&encoded_caps));
        app_sink.set_max_buffers(RAW_AUDIO_QUEUE_BUFFERS);
        app_sink.set_drop(true);
        app_sink.set_wait_on_eos(false);
        app_sink.set_sync(false);

        pipeline
            .add_many([
                &source,
                &queue,
                &convert,
                &resample,
                &raw_caps_filter,
                &encoder,
                &encoded_caps_filter,
                app_sink.upcast_ref(),
            ])
            .map_err(|error| {
                MediaGraphError::new(format!("assemble backend audio pipeline: {error}"))
            })?;
        gst::Element::link_many([
            &source,
            &queue,
            &convert,
            &resample,
            &raw_caps_filter,
            &encoder,
            &encoded_caps_filter,
            app_sink.upcast_ref(),
        ])
        .map_err(|error| MediaGraphError::new(format!("link backend audio pipeline: {error}")))?;

        Ok(Self {
            app_sink,
            expected_encoded_caps: encoded_caps,
            effective_encoded_caps: None,
            encoded_packets: 0,
            timestamp_state,
            _remote: input.remote,
        })
    }

    pub(crate) fn try_pull_packet(
        &mut self,
    ) -> Result<Option<RawEncodedAudioPacket>, MediaGraphError> {
        if let Some(error) = self
            .timestamp_state
            .lock()
            .expect("raw audio timestamp mutex poisoned")
            .failure
            .clone()
        {
            return Err(error);
        }
        let Some(sample) = self.app_sink.try_pull_sample(gst::ClockTime::ZERO) else {
            return Ok(None);
        };
        self.consume_sample(&sample).map(Some)
    }

    pub(crate) fn begin_segment(&mut self) {
        while self
            .app_sink
            .try_pull_sample(gst::ClockTime::ZERO)
            .is_some()
        {}
        self.encoded_packets = 0;
        let mut state = self
            .timestamp_state
            .lock()
            .expect("raw audio timestamp mutex poisoned");
        state.timeline = RawAudioTimeline::default();
        state.failure = None;
    }

    pub(crate) fn wake_worker_on_output(&self, worker: std::thread::Thread) {
        self.timestamp_state
            .lock()
            .expect("raw audio timestamp mutex poisoned")
            .worker = Some(worker.clone());
        let preroll_worker = worker.clone();
        let sample_worker = worker.clone();
        self.app_sink.set_callbacks(
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

    fn consume_sample(
        &mut self,
        sample: &gst::Sample,
    ) -> Result<RawEncodedAudioPacket, MediaGraphError> {
        let caps = sample
            .caps()
            .ok_or_else(|| MediaGraphError::new("encoded audio sample has no caps"))?;
        if !caps.can_intersect(&self.expected_encoded_caps) {
            return Err(MediaGraphError::new(format!(
                "encoded audio sample caps {caps} differ from required {}",
                self.expected_encoded_caps
            )));
        }
        let effective_caps = caps.to_string();
        match &self.effective_encoded_caps {
            Some(previous) if previous != &effective_caps => {
                return Err(MediaGraphError::new(format!(
                    "encoded audio caps changed within a generation from {previous} to {effective_caps}"
                )));
            }
            None => self.effective_encoded_caps = Some(effective_caps.clone()),
            Some(_) => {}
        }
        let buffer = sample
            .buffer()
            .ok_or_else(|| MediaGraphError::new("encoded audio sample has no buffer"))?;
        let pts = buffer.pts().map(|pts| pts.nseconds()).ok_or_else(|| {
            MediaGraphError::new("encoded audio packet has no presentation timestamp")
        })?;
        let duration = buffer
            .duration()
            .map(|duration| Duration::from_nanos(duration.nseconds()))
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| MediaGraphError::new("encoded audio packet has no duration"))?;
        validate_opus_packet_duration(
            self.encoded_packets,
            duration,
            buffer.flags().contains(gst::BufferFlags::DISCONT),
        )?;
        let mapped = buffer.map_readable().map_err(|error| {
            MediaGraphError::new(format!("map encoded audio packet read-only: {error}"))
        })?;
        if mapped.is_empty() || mapped.len() > MAX_ENCODED_AUDIO_PACKET_BYTES {
            return Err(MediaGraphError::new(format!(
                "encoded audio packet size {} is outside 1..={MAX_ENCODED_AUDIO_PACKET_BYTES}",
                mapped.len()
            )));
        }
        self.encoded_packets = self.encoded_packets.saturating_add(1);
        Ok(RawEncodedAudioPacket {
            data: mapped.as_slice().to_vec(),
            pts,
            duration,
            caps: effective_caps,
        })
    }
}

fn validate_opus_packet_duration(
    packet_index: u64,
    duration: Duration,
    discontinuity: bool,
) -> Result<(), MediaGraphError> {
    if duration == OPUS_FRAME_DURATION
        || (packet_index == 0 && discontinuity && duration == OPUS_PRIMED_FRAME_DURATION)
    {
        return Ok(());
    }
    Err(MediaGraphError::new(format!(
        "encoded audio packet {packet_index} duration is {duration:?}; expected {OPUS_FRAME_DURATION:?}, or the initial discontinuous Opus priming duration {OPUS_PRIMED_FRAME_DURATION:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        audio_stream_properties, validate_opus_packet_duration, RawAudioTimeline, RawAudioTiming,
        OPUS_FRAME_DURATION, OPUS_PRIMED_FRAME_DURATION,
    };

    #[test]
    fn raw_audio_timeline_uses_the_first_source_timestamp_and_sample_count() {
        let mut timeline = RawAudioTimeline::default();

        assert_eq!(
            timeline.next(Some(7_000_000_000), 1_920).unwrap(),
            RawAudioTiming {
                presentation_timestamp_ns: 7_000_000_000,
                duration_ns: 10_000_000,
                first_frame: 0,
                end_frame: 480,
                discontinuity: true,
            }
        );
        assert_eq!(
            timeline.next(Some(1), 1_920).unwrap(),
            RawAudioTiming {
                presentation_timestamp_ns: 7_010_000_000,
                duration_ns: 10_000_000,
                first_frame: 480,
                end_frame: 960,
                discontinuity: false,
            }
        );
    }

    #[test]
    fn raw_audio_timeline_rejects_missing_or_partial_input() {
        assert!(RawAudioTimeline::default().next(None, 1_920).is_err());
        assert!(RawAudioTimeline::default().next(Some(1), 0).is_err());
        assert!(RawAudioTimeline::default().next(Some(1), 1_919).is_err());
    }

    #[test]
    fn targets_private_audio_source_instead_of_sink_monitor() {
        gstreamer::init().unwrap();
        let properties = audio_stream_properties();

        assert_eq!(
            properties.get::<String>("node.dont-fallback").unwrap(),
            "true"
        );
        assert_eq!(
            properties.get::<String>("node.dont-reconnect").unwrap(),
            "true"
        );
        assert!(!properties.has_field("stream.is-live"));
        assert!(!properties.has_field("stream.capture.sink"));
    }

    #[test]
    fn accepts_only_the_initial_discontinuous_opus_priming_duration() {
        validate_opus_packet_duration(0, OPUS_PRIMED_FRAME_DURATION, true).unwrap();
        validate_opus_packet_duration(0, OPUS_FRAME_DURATION, false).unwrap();
        validate_opus_packet_duration(1, OPUS_FRAME_DURATION, false).unwrap();

        assert!(validate_opus_packet_duration(0, OPUS_PRIMED_FRAME_DURATION, false).is_err());
        assert!(validate_opus_packet_duration(1, OPUS_PRIMED_FRAME_DURATION, true).is_err());
        assert!(
            validate_opus_packet_duration(0, std::time::Duration::from_millis(10), true).is_err()
        );
    }
}
