use pronk_dbus::{
    CastDisplayInfo, CastDisplayState, DisplayAttachmentState, DisplayIdentitySource,
    DisplayRouteState, MediaSessionPhase, MediaSessionState, OperationStage, OperationState,
    PnpResolutionSource, RoutedDisplayMode, MAX_MEDIA_ERROR_BYTES,
};

use crate::display::{DisplaySetupSnapshot, DisplaySetupStage};

pub(super) fn public_operation_state(snapshot: &DisplaySetupSnapshot) -> OperationState {
    OperationState {
        display_id: snapshot.display_id.to_string(),
        stage: match snapshot.stage {
            DisplaySetupStage::Validating => OperationStage::Validating,
            DisplaySetupStage::Authorizing => OperationStage::Authorizing,
            DisplaySetupStage::PreparingDevice => OperationStage::PreparingDevice,
            DisplaySetupStage::Attaching => OperationStage::Attaching,
            DisplaySetupStage::Added => OperationStage::Added,
            DisplaySetupStage::Cancelled => OperationStage::Cancelled,
            DisplaySetupStage::Failed => OperationStage::Failed,
        },
        error_code: snapshot.error_code,
        error: snapshot.error.clone().unwrap_or_default(),
    }
}

pub(super) fn public_display(
    display: &crate::display::AddedCastDisplaySnapshot,
) -> CastDisplayInfo {
    let identity = &display.prepared.capabilities().display_identity;
    let numeric_identity = display.prepared.generated_edid().identity();
    CastDisplayInfo {
        display_id: display.display_id.to_string(),
        backend_id: display.device.backend_id.clone(),
        device_id: display.device.device_id.clone(),
        display_name: display.device.display_name.clone(),
        manufacturer_name: identity.manufacturer_name.clone().unwrap_or_default(),
        manufacturer_source: public_identity_source(identity.manufacturer_source),
        product_name: identity.product_name.clone().unwrap_or_default(),
        product_source: public_identity_source(identity.product_source),
        pnp_id: display.prepared.pnp_resolution().pnp_id.to_string(),
        pnp_resolution_source: match display.prepared.pnp_resolution().source {
            pronk_core::identity::PnpResolutionSource::AuthenticatedPnpId => {
                PnpResolutionSource::AuthenticatedPnpId
            }
            pronk_core::identity::PnpResolutionSource::ExactName => PnpResolutionSource::ExactName,
            pronk_core::identity::PnpResolutionSource::LegalSuffixName => {
                PnpResolutionSource::LegalSuffixName
            }
            pronk_core::identity::PnpResolutionSource::ReviewedAlias => {
                PnpResolutionSource::ReviewedAlias
            }
            pronk_core::identity::PnpResolutionSource::SynthesizerFallback => {
                PnpResolutionSource::SynthesizerFallback
            }
        },
        connector_id: display.output.connector_id,
        connector_name: display.output.connector_name.clone(),
        output_index: display.output.id.output_index,
        product_code: numeric_identity.product_code,
        serial: numeric_identity.serial,
        attachment_state: public_attachment_state(display.runtime.attachment()),
    }
}

pub(super) fn public_display_state(
    display: &crate::display::AddedCastDisplaySnapshot,
) -> CastDisplayState {
    CastDisplayState {
        revision: display.state_revision,
        device: display.device.clone(),
        attachment_state: public_attachment_state(display.runtime.attachment()),
        route_state: match display.runtime.route() {
            crate::display_state::RouteState::Disabled => DisplayRouteState::Disabled,
            crate::display_state::RouteState::Active(_) => DisplayRouteState::Active,
        },
        routed_mode: match display.runtime.route() {
            crate::display_state::RouteState::Disabled => None,
            crate::display_state::RouteState::Active(route) => Some(RoutedDisplayMode {
                width: route.mode.width,
                height: route.mode.height,
                refresh_millihz: route.mode.refresh_millihz,
                flags: route.mode.flags,
            }),
        },
    }
}

pub(super) fn public_media_session_state(
    display: &crate::display::AddedCastDisplaySnapshot,
) -> MediaSessionState {
    let phase = match display.runtime.media() {
        crate::display_state::MediaState::Idle => MediaSessionPhase::Inactive,
        crate::display_state::MediaState::StartingCapture
        | crate::display_state::MediaState::StartingMedia => MediaSessionPhase::Starting,
        crate::display_state::MediaState::Running => MediaSessionPhase::Running,
        crate::display_state::MediaState::Suspended => MediaSessionPhase::Suspended,
        crate::display_state::MediaState::Reconfiguring
        | crate::display_state::MediaState::Reconnecting => MediaSessionPhase::Recovering,
        crate::display_state::MediaState::Stopping => MediaSessionPhase::Stopping,
        crate::display_state::MediaState::Failed => MediaSessionPhase::Failed,
    };
    let error = if phase == MediaSessionPhase::Failed {
        public_media_error(display.runtime.last_error())
    } else {
        String::new()
    };
    let state = MediaSessionState {
        revision: display.state_revision,
        phase,
        media_generation: display.runtime.media_generation(),
        audio_enabled: display.prepared.audio_enabled(),
        error,
    };
    debug_assert!(state.validate().is_ok());
    state
}

fn public_media_error(error: Option<&str>) -> String {
    const MISSING_DIAGNOSTIC: &str = "media session failed without diagnostic detail";

    let mut error = error
        .unwrap_or(MISSING_DIAGNOSTIC)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned();
    if error.is_empty() {
        return MISSING_DIAGNOSTIC.into();
    }
    if error.len() > MAX_MEDIA_ERROR_BYTES {
        let mut boundary = MAX_MEDIA_ERROR_BYTES;
        while !error.is_char_boundary(boundary) {
            boundary -= 1;
        }
        error.truncate(boundary);
    }
    error
}

pub(super) fn same_media_observation(left: &MediaSessionState, right: &MediaSessionState) -> bool {
    left.phase == right.phase
        && left.media_generation == right.media_generation
        && left.audio_enabled == right.audio_enabled
        && left.error == right.error
}

fn public_attachment_state(state: crate::display_state::AttachmentState) -> DisplayAttachmentState {
    match state {
        crate::display_state::AttachmentState::Attached => DisplayAttachmentState::Attached,
        crate::display_state::AttachmentState::Detached => DisplayAttachmentState::Detached,
        crate::display_state::AttachmentState::Unknown => DisplayAttachmentState::Unknown,
    }
}

fn public_identity_source(source: pronk_backend_protocol::IdentitySource) -> DisplayIdentitySource {
    match source {
        pronk_backend_protocol::IdentitySource::Absent => DisplayIdentitySource::Absent,
        pronk_backend_protocol::IdentitySource::SetupEndpoint => {
            DisplayIdentitySource::SetupEndpoint
        }
        pronk_backend_protocol::IdentitySource::AuthenticatedDeviceInfo => {
            DisplayIdentitySource::AuthenticatedDeviceInfo
        }
        pronk_backend_protocol::IdentitySource::DiscoveryAdvertisement => {
            DisplayIdentitySource::DiscoveryAdvertisement
        }
    }
}
