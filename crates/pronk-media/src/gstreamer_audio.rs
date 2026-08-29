//! Connector-bound PipeWire sink-monitor capture and Opus encoding branch.

use std::os::fd::{AsFd, AsRawFd};
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
// opusenc reports its restricted-low-delay lookahead as clipping on the first
// encoded packet. The packet still contains one configured 20 ms Opus frame,
// but its effective GStreamer timeline duration is shorter by 2.5 ms.
const OPUS_PRIMED_FRAME_DURATION: Duration = Duration::from_micros(17_500);

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
        let stream_properties = gst::Structure::builder("pronk-pipewire-audio-stream")
            .field("node.dont-fallback", "true")
            .field("node.dont-reconnect", "true")
            .field("stream.capture.sink", "true")
            .build();
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
            _remote: input.remote,
        })
    }

    pub(crate) fn try_pull_packet(
        &mut self,
    ) -> Result<Option<RawEncodedAudioPacket>, MediaGraphError> {
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
    }

    pub(crate) fn wake_worker_on_output(&self, worker: std::thread::Thread) {
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
    use super::{validate_opus_packet_duration, OPUS_FRAME_DURATION, OPUS_PRIMED_FRAME_DURATION};

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
