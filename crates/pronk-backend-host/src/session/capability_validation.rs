use pronk_backend_protocol::{DeviceCapabilities, PreparationRequest};

use super::BackendSessionError;

pub(super) fn validate_capabilities_against_offer(
    offer: &PreparationRequest,
    capabilities: &DeviceCapabilities,
) -> Result<(), BackendSessionError> {
    if capabilities.features & !offer.requested_features != 0 {
        return Err(BackendSessionError::CapabilitiesOutsideOffer(
            "feature bits",
        ));
    }
    if capabilities
        .modes
        .iter()
        .any(|mode| !offer.candidate_modes.contains(mode))
    {
        return Err(BackendSessionError::CapabilitiesOutsideOffer(
            "display mode",
        ));
    }
    for returned in &capabilities.video_profiles {
        let Some(offered) = offer
            .video_profiles
            .iter()
            .find(|offered| offered.profile_id == returned.profile_id)
        else {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "video profile ID",
            ));
        };
        if returned.codec != offered.codec
            || returned.max_width > offered.max_width
            || returned.max_height > offered.max_height
            || returned.max_refresh_millihz > offered.max_refresh_millihz
        {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "video profile limits",
            ));
        }
        if returned
            .raw_layouts
            .iter()
            .any(|layout| !offered.raw_layouts.contains(layout))
        {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "raw video layout",
            ));
        }
        if capabilities
            .modes
            .iter()
            .filter(|mode| returned.supports_mode(mode))
            .any(|mode| {
                returned
                    .raw_layouts
                    .iter()
                    .any(|layout| !offer.supports_layout(mode, layout))
            })
        {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "mode raw video layout",
            ));
        }
    }
    for returned in &capabilities.audio_profiles {
        let Some(offered) = offer
            .audio_profiles
            .iter()
            .find(|offered| offered.profile_id == returned.profile_id)
        else {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "audio profile ID",
            ));
        };
        if returned.codec != offered.codec
            || returned.max_channels > offered.max_channels
            || returned
                .sample_rates
                .iter()
                .any(|rate| !offered.sample_rates.contains(rate))
        {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "audio profile limits",
            ));
        }
    }
    Ok(())
}
