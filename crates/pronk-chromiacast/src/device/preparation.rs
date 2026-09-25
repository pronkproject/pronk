use pronk_backend_protocol::{
    AudioProfile, DeviceCapabilities, DisplayIdentity, DisplayMode, ModeRawLayouts,
    PreparationRequest, RawVideoLayout, Validate, VideoProfile, SESSION_FEATURE_AUDIO,
    SESSION_FEATURE_CONTROL,
};
use pronk_media::OPUS_SAMPLE_RATE;

use super::DeviceActorError;

pub(super) fn retain_supported_modes(
    request: &mut PreparationRequest,
    supported_modes: Vec<DisplayMode>,
) {
    request
        .mode_raw_layouts
        .retain(|entry| supported_modes.contains(&entry.mode));
    request.candidate_modes = supported_modes;
}

pub(super) fn retain_supported_layouts(
    request: &mut PreparationRequest,
    supported_layouts: Vec<Vec<RawVideoLayout>>,
) {
    debug_assert_eq!(request.candidate_modes.len(), supported_layouts.len());
    let offered = std::mem::take(&mut request.mode_raw_layouts);
    let mut usable = Vec::new();
    for (mode, supported) in request
        .candidate_modes
        .iter()
        .copied()
        .zip(supported_layouts)
    {
        let source = if offered.is_empty() {
            request
                .video_profiles
                .iter()
                .flat_map(|profile| profile.raw_layouts.iter().copied())
                .collect::<Vec<_>>()
        } else {
            offered
                .iter()
                .find(|entry| entry.mode == mode)
                .map(|entry| entry.raw_layouts.clone())
                .unwrap_or_default()
        };
        let mut raw_layouts = Vec::new();
        for layout in source {
            if supported.contains(&layout) && !raw_layouts.contains(&layout) {
                raw_layouts.push(layout);
            }
        }
        if !raw_layouts.is_empty() {
            usable.push(ModeRawLayouts { mode, raw_layouts });
        }
    }
    let modes = usable.iter().map(|entry| entry.mode).collect();
    request.mode_raw_layouts = usable;
    retain_supported_modes(request, modes);
}

pub(super) fn negotiate_capabilities(
    request: PreparationRequest,
    display_identity: DisplayIdentity,
    raw_layouts: &[RawVideoLayout],
) -> Result<DeviceCapabilities, DeviceActorError> {
    let audio_requested = request.requested_features & SESSION_FEATURE_AUDIO != 0;
    let control_requested = request.requested_features & SESSION_FEATURE_CONTROL != 0;
    let candidate_modes: Vec<_> = request
        .candidate_modes
        .iter()
        .copied()
        .filter(supported_sender_mode)
        .collect();
    if candidate_modes.is_empty() {
        return Err(DeviceActorError::NoSupportedMode);
    }
    let video_profile = request
        .video_profiles
        .iter()
        .enumerate()
        .filter_map(|(index, profile)| {
            narrow_h264_profile(profile.clone(), raw_layouts, &candidate_modes, &request)
                .map(|profile| (index, profile))
        })
        .max_by_key(|(index, profile)| {
            let layout = profile.raw_layouts[0];
            let modes = candidate_modes
                .iter()
                .filter(|mode| {
                    profile.supports_mode(mode) && request.supports_layout(mode, &layout)
                })
                .count();
            (modes, std::cmp::Reverse(*index))
        })
        .map(|(_, profile)| profile)
        .ok_or(DeviceActorError::NoSupportedVideoProfile)?;
    let selected_layout = video_profile.raw_layouts[0];
    let modes = candidate_modes
        .into_iter()
        .filter(|mode| {
            video_profile.supports_mode(mode) && request.supports_layout(mode, &selected_layout)
        })
        .collect();
    let audio_profiles: Vec<_> = if audio_requested {
        request
            .audio_profiles
            .iter()
            .cloned()
            .filter_map(narrow_opus_profile)
            .take(1)
            .collect()
    } else {
        Vec::new()
    };
    if audio_requested && audio_profiles.is_empty() {
        return Err(DeviceActorError::NoSupportedAudioProfile);
    }
    let capabilities = DeviceCapabilities {
        preparation_generation: request.preparation_generation,
        display_identity,
        modes,
        video_profiles: vec![video_profile],
        audio_profiles,
        features: (u64::from(audio_requested) * SESSION_FEATURE_AUDIO)
            | (u64::from(control_requested) * SESSION_FEATURE_CONTROL),
    };
    capabilities
        .validate()
        .map_err(|error| DeviceActorError::InvalidRequest(error.to_string()))?;
    Ok(capabilities)
}

fn narrow_opus_profile(profile: AudioProfile) -> Option<AudioProfile> {
    if profile.codec != "opus"
        || profile.max_channels < 2
        || !profile.sample_rates.contains(&OPUS_SAMPLE_RATE)
    {
        return None;
    }
    Some(AudioProfile {
        profile_id: "opus-stereo".into(),
        codec: "opus".into(),
        max_channels: 2,
        sample_rates: vec![OPUS_SAMPLE_RATE],
    })
}

fn narrow_h264_profile(
    mut profile: VideoProfile,
    raw_layouts: &[RawVideoLayout],
    modes: &[DisplayMode],
    request: &PreparationRequest,
) -> Option<VideoProfile> {
    if profile.codec != "h264" {
        return None;
    }
    profile.max_width = profile.max_width.min(3_840);
    profile.max_height = profile.max_height.min(2_160);
    profile.max_refresh_millihz = profile.max_refresh_millihz.min(60_000);
    let raw_layout = profile
        .raw_layouts
        .iter()
        .enumerate()
        .filter(|(_, layout)| raw_layouts.contains(layout))
        .map(|(index, layout)| {
            let mode_count = modes
                .iter()
                .filter(|mode| profile.supports_mode(mode) && request.supports_layout(mode, layout))
                .count();
            (index, *layout, mode_count)
        })
        .filter(|(_, _, mode_count)| *mode_count > 0)
        .max_by_key(|(index, _, mode_count)| (*mode_count, std::cmp::Reverse(*index)))?
        .1;
    profile.raw_layouts = vec![raw_layout];
    Some(profile)
}

fn supported_sender_mode(mode: &DisplayMode) -> bool {
    if mode.flags != 0 {
        return false;
    }

    // Cast transports only the encoded picture; DRM blanking and sync flags do
    // not reach the receiver. Keep presentation modes at the receiver's 16:9
    // aspect so Cast applications cannot crop wider or taller desktops while
    // fitting the video to the television. CTA EDIDs still require the VGA
    // compatibility timing, so retain that one fallback until the media path
    // can letterbox it explicitly.
    matches!(
        (mode.width, mode.height, mode.refresh_millihz),
        (3_840, 2_160, 30_000)
            | (2_560, 1_440, 60_000)
            | (1_920, 1_080, 60_000)
            | (1_600, 900, 60_000)
            | (1_366, 768, 60_000)
            | (1_280, 720, 60_000)
            | (640, 480, 60_000)
    )
}
