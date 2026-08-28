//! Backend-local GStreamer media ownership.
//!
//! This crate consumes only already-connected PipeWire client descriptors and
//! exact media targets. It has no D-Bus, device-protocol, CastKMS, or ambient
//! PipeWire authority. A dedicated actor thread owns all pipeline mutation and
//! bus handling; Tokio callers exchange bounded generation-scoped commands.

mod actor;
mod encoded_output;
mod gstreamer_audio;
mod gstreamer_graph;
mod h264;
mod media_timeline;
mod model;

pub use actor::{EncodedMediaReceivers, MediaGraphActor};
pub use model::{
    EncodedAudioPacket, EncodedVideoAccessUnit, MediaGraphConfiguration, MediaGraphError,
    MediaGraphSnapshot, MediaGraphState, MediaGraphStatistics, PipeWireAudioInput,
    PipeWireVideoInput, ValidatedAudioCaps, ValidatedVideoCaps, VideoFrameDependency,
    MAX_ENCODED_ACCESS_UNIT_BYTES, MAX_ENCODED_AUDIO_PACKET_BYTES, MAX_ENCODED_OUTPUT_CAPACITY,
    OPUS_BITRATE, OPUS_CHANNELS, OPUS_SAMPLE_RATE,
};
