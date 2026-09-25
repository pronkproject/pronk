use std::num::NonZeroU64;
use std::os::fd::OwnedFd as StdOwnedFd;

use pronk_backend_protocol::{
    DeviceCapabilities, MediaConfiguration, MediaKind, PipeWireTarget, RawVideoLayout,
    SESSION_FEATURE_AUDIO,
};
use pronk_media::{
    DrmVideoFormat, MediaGraphConfiguration, MediaGraphError, PipeWireAudioInput,
    PipeWireVideoInput, ValidatedAudioCaps, ValidatedVideoCaps, VideoCadence, VideoCodec,
    VideoInputLayout, OPUS_BITRATE, OPUS_CHANNELS, OPUS_SAMPLE_RATE,
};
use zbus::zvariant::OwnedFd;

use super::encoder_policy::VideoEncoderPolicy;
use super::{chromecast_video_cadence, minimum_playout_delay, MediaSessionError};
use crate::feedback::INITIAL_PLAYOUT_DELAY;
use crate::transport::{AudioTransportConfiguration, VideoTransportConfiguration};

#[derive(Debug)]
pub(super) struct PendingMediaGraphConfiguration {
    media_generation: NonZeroU64,
    video: PipeWireVideoInput,
    pub(super) audio: Option<PipeWireAudioInput>,
    video_cadence: VideoCadence,
    pub(super) video_bitrate: NonZeroU64,
}

impl PendingMediaGraphConfiguration {
    pub(super) fn with_encoder(
        self,
        policy: &VideoEncoderPolicy,
        video_codec: VideoCodec,
    ) -> Result<MediaGraphConfiguration, MediaGraphError> {
        Ok(MediaGraphConfiguration {
            media_generation: self.media_generation,
            video: self.video,
            audio: self.audio,
            video_encoder: policy.encoder(video_codec)?,
            video_cadence: self.video_cadence,
            video_bitrate: self.video_bitrate,
        })
    }
}

pub(super) fn graph_configuration(
    session_id: &str,
    capabilities: &DeviceCapabilities,
    encoder_policy: &VideoEncoderPolicy,
    remotes: Vec<OwnedFd>,
    targets: Vec<PipeWireTarget>,
    configuration: MediaConfiguration,
    generation: NonZeroU64,
) -> Result<(PendingMediaGraphConfiguration, VideoTransportConfiguration), MediaSessionError> {
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
    let negotiated_mode = capabilities
        .modes
        .iter()
        .find(|mode| mode.matches_realized(&configuration.mode))
        .ok_or_else(|| {
            MediaSessionError::InvalidRequest(
                "configured mode was not negotiated by Prepare".into(),
            )
        })?;
    let profile = capabilities
        .video_profiles
        .iter()
        .find(|profile| profile.profile_id == configuration.video_profile_id)
        .ok_or_else(|| {
            MediaSessionError::InvalidRequest(
                "configured video profile was not negotiated by Prepare".into(),
            )
        })?;
    if !profile.supports_mode(negotiated_mode) {
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
    if video_target.session_id != session_id {
        return Err(MediaSessionError::InvalidRequest(
            "PipeWire target belongs to another session".into(),
        ));
    }
    encoder_policy
        .validate_video_target(video_target.render_device)
        .map_err(MediaSessionError::InvalidRequest)?;
    let caps = ValidatedVideoCaps::parse(&video_target.caps)?;
    let raw_layout = raw_layout_from_caps(&caps)?;
    if !profile.raw_layouts.contains(&raw_layout) {
        return Err(MediaSessionError::InvalidRequest(
            "video target does not use a negotiated raw-video layout".into(),
        ));
    }
    if caps.width.get() != configuration.mode.width
        || caps.height.get() != configuration.mode.height
    {
        return Err(MediaSessionError::InvalidRequest(format!(
            "video caps are {}x{} but configured mode is {}x{}",
            caps.width, caps.height, configuration.mode.width, configuration.mode.height
        )));
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
    encoder_policy
        .validate_bitrate(configuration.video_bitrate)
        .map_err(MediaSessionError::InvalidRequest)?;
    let video_cadence = chromecast_video_cadence();
    if !caps.supports_cadence(video_cadence) {
        return Err(MediaSessionError::InvalidRequest(format!(
            "video caps cadence {}/{} is below the required {}/{}",
            caps.framerate_numerator,
            caps.framerate_denominator,
            video_cadence.numerator,
            video_cadence.denominator
        )));
    }
    let minimum_playout_delay = minimum_playout_delay(
        video_cadence.numerator.get(),
        video_cadence.denominator.get(),
        audio.is_some(),
    );
    let transport = VideoTransportConfiguration {
        width: caps.width.get(),
        height: caps.height.get(),
        framerate_numerator: video_cadence.numerator.get(),
        framerate_denominator: video_cadence.denominator.get(),
        bitrate,
        target_playout_delay: INITIAL_PLAYOUT_DELAY.max(minimum_playout_delay),
        offer: encoder_policy.offer(),
        audio: audio.as_ref().map(|(_, transport)| *transport),
    };
    let graph = PendingMediaGraphConfiguration {
        media_generation: generation,
        video: PipeWireVideoInput {
            remote: video_remote,
            node_name: video_target.node_name,
            object_serial: NonZeroU64::new(video_target.object_serial)
                .expect("wire validation rejected zero object serial"),
            caps: video_target.caps,
        },
        audio: audio.map(|(input, _)| input),
        video_cadence,
        video_bitrate: NonZeroU64::new(configuration.video_bitrate)
            .expect("wire validation rejected zero bitrate"),
    };
    Ok((graph, transport))
}

fn raw_layout_from_caps(caps: &ValidatedVideoCaps) -> Result<RawVideoLayout, MediaSessionError> {
    Ok(match &caps.layout {
        VideoInputLayout::SystemMemoryBgrx => {
            RawVideoLayout::system_memory(u32::from_le_bytes(*b"XR24"))
        }
        VideoInputLayout::DmaBuf {
            drm_format: DrmVideoFormat { format, modifier },
        } => RawVideoLayout::dma_buf(*format, *modifier),
    })
}
