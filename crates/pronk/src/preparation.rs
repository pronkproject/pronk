//! Conversion of bounded backend capabilities into a stable monitor identity.

use std::collections::HashSet;

use pronk_backend_protocol::{
    AudioProfile, DeviceCapabilities, DisplayMode, IdentitySource, PreparationRequest, Validate,
    VideoProfile, SESSION_FEATURE_AUDIO, SESSION_FEATURE_CONTROL,
};
use pronk_core::edid::{
    build_cast_display_edid, CastDisplayEdidError, CastDisplayEdidRequest, EdidMode,
    GeneratedCastDisplayEdid, MAX_INITIAL_EDID_MODES,
};
use pronk_core::identity::{PnpIdError, PnpIdResolver, ResolvedPnpId};
use pronk_dbus::{DeviceAvailability, DeviceInfo};
use thiserror::Error;

/// Conservative version-1 offer supported end-to-end by the initial EDID and
/// software-encoder path.
pub fn initial_preparation_offer(audio_enabled: bool) -> PreparationRequest {
    PreparationRequest {
        preparation_generation: 1,
        candidate_modes: vec![
            DisplayMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 1280,
                height: 720,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 640,
                height: 480,
                refresh_millihz: 60_000,
                flags: 0,
            },
        ],
        video_profiles: vec![VideoProfile {
            profile_id: "h264-high".into(),
            codec: "h264".into(),
            max_width: 1920,
            max_height: 1080,
            max_refresh_millihz: 60_000,
        }],
        audio_profiles: if audio_enabled {
            vec![AudioProfile {
                profile_id: "opus-stereo".into(),
                codec: "opus".into(),
                max_channels: 2,
                sample_rates: vec![48_000],
            }]
        } else {
            Vec::new()
        },
        requested_features: SESSION_FEATURE_CONTROL
            | if audio_enabled {
                SESSION_FEATURE_AUDIO
            } else {
                0
            },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedIdentityProvenance {
    pub manufacturer_source: IdentitySource,
    pub product_source: IdentitySource,
    pub display_name_omitted_from_edid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCastDevice {
    device: DeviceInfo,
    capabilities: DeviceCapabilities,
    pnp_resolution: ResolvedPnpId,
    provenance: PreparedIdentityProvenance,
    generated_edid: GeneratedCastDisplayEdid,
}

impl PreparedCastDevice {
    pub fn from_capabilities(
        device: DeviceInfo,
        capabilities: DeviceCapabilities,
        pnp_resolver: &PnpIdResolver,
        audio_enabled: bool,
    ) -> Result<Self, PrepareCastDeviceError> {
        device
            .validate()
            .map_err(|error| PrepareCastDeviceError::InvalidDevice(error.to_string()))?;
        if device.availability != DeviceAvailability::Available {
            return Err(PrepareCastDeviceError::DeviceUnavailable(
                device.availability,
            ));
        }
        capabilities
            .validate()
            .map_err(|error| PrepareCastDeviceError::InvalidCapabilities(error.to_string()))?;

        let display_identity = &capabilities.display_identity;
        require_usable_identity_source(
            "manufacturer",
            display_identity.manufacturer_name.as_deref(),
            display_identity.manufacturer_source,
        )?;
        require_usable_identity_source(
            "product",
            display_identity.product_name.as_deref(),
            display_identity.product_source,
        )?;
        let pnp_resolution = pnp_resolver.resolve(
            display_identity.pnp_id.as_deref(),
            display_identity.manufacturer_name.as_deref(),
        )?;
        let product_name = display_identity
            .product_name
            .as_deref()
            .and_then(edid_safe_product_name);
        let display_name = edid_safe_product_name(&device.display_name);
        let display_name_omitted_from_edid = display_name.is_none();
        let modes = select_initial_modes(&capabilities)?;
        let audio = audio_enabled && capabilities.features & SESSION_FEATURE_AUDIO != 0;
        let control = capabilities.features & SESSION_FEATURE_CONTROL != 0;
        let generated_edid = build_cast_display_edid(CastDisplayEdidRequest {
            pnp_id: pnp_resolution.pnp_id,
            manufacturer_name: display_identity.manufacturer_name.clone(),
            product_name,
            display_name,
            backend_id: device.backend_id.clone(),
            device_id: device.device_id.clone(),
            modes,
            audio,
            cec_physical_address: control.then_some(0x1000),
        })?;
        let provenance = PreparedIdentityProvenance {
            manufacturer_source: display_identity.manufacturer_source,
            product_source: display_identity.product_source,
            display_name_omitted_from_edid,
        };

        Ok(Self {
            device,
            capabilities,
            pnp_resolution,
            provenance,
            generated_edid,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub fn capabilities(&self) -> &DeviceCapabilities {
        &self.capabilities
    }

    pub fn pnp_resolution(&self) -> &ResolvedPnpId {
        &self.pnp_resolution
    }

    pub fn provenance(&self) -> &PreparedIdentityProvenance {
        &self.provenance
    }

    pub fn generated_edid(&self) -> &GeneratedCastDisplayEdid {
        &self.generated_edid
    }

    pub fn audio_enabled(&self) -> bool {
        self.capabilities.features & SESSION_FEATURE_AUDIO != 0
            && !self.capabilities.audio_profiles.is_empty()
    }

    pub fn control_enabled(&self) -> bool {
        self.capabilities.features & SESSION_FEATURE_CONTROL != 0
    }

    /// Require a freshly prepared session to preserve the already attached
    /// monitor configuration and every media profile selected by the core.
    ///
    /// Discovery generations and presentation text may change independently,
    /// The attached EDID keeps its original presentation name during an
    /// in-place recovery; all other EDID inputs and selected profiles must
    /// remain compatible with the retained capture pipeline.
    pub fn validate_recovery(
        &self,
        replacement: &PreparedCastDevice,
    ) -> Result<(), PreparedDeviceRecoveryError> {
        if self.device.backend_id != replacement.device.backend_id
            || self.device.device_id != replacement.device.device_id
        {
            return Err(PreparedDeviceRecoveryError::DifferentDevice);
        }
        if !self
            .generated_edid
            .has_same_monitor_configuration(&replacement.generated_edid)
        {
            return Err(PreparedDeviceRecoveryError::EdidChanged);
        }

        let video = self
            .capabilities
            .video_profiles
            .first()
            .expect("prepared Device always has a selected video profile");
        if replacement
            .capabilities
            .video_profiles
            .iter()
            .find(|candidate| candidate.profile_id == video.profile_id)
            != Some(video)
        {
            return Err(PreparedDeviceRecoveryError::VideoProfileChanged(
                video.profile_id.clone(),
            ));
        }

        if let Some(audio) = self.capabilities.audio_profiles.first() {
            if replacement
                .capabilities
                .audio_profiles
                .iter()
                .find(|candidate| candidate.profile_id == audio.profile_id)
                != Some(audio)
            {
                return Err(PreparedDeviceRecoveryError::AudioProfileChanged(
                    audio.profile_id.clone(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PreparedDeviceRecoveryError {
    #[error("replacement preparation resolved a different Device identity")]
    DifferentDevice,
    #[error("replacement preparation would change the attached EDID")]
    EdidChanged,
    #[error("replacement preparation changed required video profile {0:?}")]
    VideoProfileChanged(String),
    #[error("replacement preparation changed required audio profile {0:?}")]
    AudioProfileChanged(String),
}

fn require_usable_identity_source(
    field: &'static str,
    value: Option<&str>,
    source: IdentitySource,
) -> Result<(), PrepareCastDeviceError> {
    if value.is_some()
        && !matches!(
            source,
            IdentitySource::SetupEndpoint | IdentitySource::AuthenticatedDeviceInfo
        )
    {
        return Err(PrepareCastDeviceError::UnsupportedIdentitySource {
            field,
            identity_source: source,
        });
    }
    Ok(())
}

fn edid_safe_product_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > pronk_core::edid::EDID_PRODUCT_NAME_MAX_BYTES
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        None
    } else {
        Some(value.into())
    }
}

fn select_initial_modes(
    capabilities: &DeviceCapabilities,
) -> Result<Vec<EdidMode>, PrepareCastDeviceError> {
    let mut supported = Vec::new();
    let mut identities = HashSet::new();
    for mode in &capabilities.modes {
        let Ok(mode) = EdidMode::new(mode.width, mode.height, mode.refresh_millihz) else {
            continue;
        };
        if identities.insert(mode) {
            supported.push(mode);
        }
    }
    if supported.is_empty() {
        return Err(PrepareCastDeviceError::NoConservativeMode);
    }
    let required = EdidMode::new(640, 480, 60_000).expect("required timing is built in");
    if !supported.contains(&required) {
        return Err(PrepareCastDeviceError::MissingRequired640x480);
    }

    let mut selected: Vec<_> = supported
        .iter()
        .copied()
        .take(MAX_INITIAL_EDID_MODES)
        .collect();
    if !selected.contains(&required) {
        *selected
            .last_mut()
            .expect("supported modes were checked nonempty") = required;
    }
    Ok(selected)
}

#[derive(Debug, Error)]
pub enum PrepareCastDeviceError {
    #[error("invalid selected Device: {0}")]
    InvalidDevice(String),
    #[error("selected Device is {0}")]
    DeviceUnavailable(DeviceAvailability),
    #[error("invalid prepared capabilities: {0}")]
    InvalidCapabilities(String),
    #[error("prepared {field} identity has unsupported source {identity_source:?}")]
    UnsupportedIdentitySource {
        field: &'static str,
        identity_source: IdentitySource,
    },
    #[error("resolve EDID manufacturer identity: {0}")]
    Pnp(#[from] PnpIdError),
    #[error("prepared Device has no mode in the conservative EDID timing set")]
    NoConservativeMode,
    #[error("prepared Device lacks required 640x480 at 60 Hz compatibility")]
    MissingRequired640x480,
    #[error("build cast-display EDID: {0}")]
    Edid(#[from] CastDisplayEdidError),
}

#[cfg(test)]
mod tests {
    use pronk_backend_protocol::{DisplayIdentity, SESSION_FEATURE_AUDIO, SESSION_FEATURE_CONTROL};
    use pronk_core::identity::{PnpResolutionSource, DEFAULT_SYNTHESIZER_PNP_ID};

    use super::*;

    fn resolver() -> PnpIdResolver {
        PnpIdResolver::from_database(
            "GGL\tGoogle Inc.\nSON\tSony\nSNY\tSony\n",
            &[],
            DEFAULT_SYNTHESIZER_PNP_ID,
        )
        .unwrap()
    }

    #[test]
    fn initial_offer_is_bounded_and_audio_is_explicit() {
        let video = initial_preparation_offer(false);
        video.validate().unwrap();
        assert_eq!(video.requested_features, SESSION_FEATURE_CONTROL);
        assert!(video.audio_profiles.is_empty());
        assert_eq!(video.candidate_modes.last().unwrap().width, 640);

        let audiovisual = initial_preparation_offer(true);
        audiovisual.validate().unwrap();
        assert_eq!(
            audiovisual.requested_features,
            SESSION_FEATURE_AUDIO | SESSION_FEATURE_CONTROL
        );
        assert_eq!(audiovisual.audio_profiles.len(), 1);
        assert_eq!(audiovisual.video_profiles[0].max_width, 1920);
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            backend_id: "mock".into(),
            device_id: "living-room".into(),
            display_name: "Living Room TV".into(),
            availability: DeviceAvailability::Available,
            connection_generation: 1,
            discovery_generation: 2,
            device_revision: 3,
            metadata: Vec::new(),
        }
    }

    fn mode(width: u32, height: u32, refresh_millihz: u32) -> DisplayMode {
        DisplayMode {
            width,
            height,
            refresh_millihz,
            flags: 0,
        }
    }

    fn capabilities() -> DeviceCapabilities {
        DeviceCapabilities {
            preparation_generation: 1,
            display_identity: DisplayIdentity {
                manufacturer_name: Some("Google".into()),
                manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
                product_name: Some("Chromecast with Google TV".into()),
                product_source: IdentitySource::SetupEndpoint,
                pnp_id: None,
            },
            modes: vec![
                mode(1920, 1080, 60_000),
                mode(1280, 720, 60_000),
                mode(640, 480, 60_000),
            ],
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 1920,
                max_height: 1080,
                max_refresh_millihz: 60_000,
            }],
            audio_profiles: vec![AudioProfile {
                profile_id: "opus-stereo".into(),
                codec: "opus".into(),
                max_channels: 2,
                sample_rates: vec![48_000],
            }],
            features: SESSION_FEATURE_AUDIO,
        }
    }

    #[test]
    fn resolves_and_embeds_prepared_identity_and_modes() {
        let prepared =
            PreparedCastDevice::from_capabilities(device(), capabilities(), &resolver(), true)
                .unwrap();
        assert_eq!(prepared.device().device_id, "living-room");
        assert_eq!(prepared.pnp_resolution().pnp_id.as_str(), "GGL");
        assert_eq!(
            prepared.pnp_resolution().source,
            PnpResolutionSource::LegalSuffixName
        );
        assert_eq!(
            prepared.generated_edid().display_name(),
            Some("Living Room TV")
        );
        assert!(!prepared.provenance().display_name_omitted_from_edid);
        assert_eq!(prepared.generated_edid().edid().len(), 384);
    }

    #[test]
    fn omits_unencodable_presentation_text_without_losing_stable_identity() {
        let mut capabilities = capabilities();
        capabilities.display_identity.product_name = Some("Téléviseur".into());
        let mut device = device();
        device.display_name = "Téléviseur".into();
        let prepared =
            PreparedCastDevice::from_capabilities(device, capabilities, &resolver(), false)
                .unwrap();
        assert_eq!(prepared.generated_edid().display_name(), None);
        assert!(prepared.provenance().display_name_omitted_from_edid);
        assert_eq!(prepared.pnp_resolution().pnp_id.as_str(), "GGL");
    }

    #[test]
    fn rejects_capabilities_without_the_required_compatibility_mode() {
        let mut capabilities = capabilities();
        capabilities.modes.retain(|mode| mode.width != 640);
        assert!(matches!(
            PreparedCastDevice::from_capabilities(device(), capabilities, &resolver(), false),
            Err(PrepareCastDeviceError::MissingRequired640x480)
        ));
    }

    #[test]
    fn rejects_discovery_advertisements_as_attached_monitor_identity() {
        let mut capabilities = capabilities();
        capabilities.display_identity.product_source = IdentitySource::DiscoveryAdvertisement;
        assert!(matches!(
            PreparedCastDevice::from_capabilities(device(), capabilities, &resolver(), false),
            Err(PrepareCastDeviceError::UnsupportedIdentitySource {
                field: "product",
                identity_source: IdentitySource::DiscoveryAdvertisement,
            })
        ));
    }

    #[test]
    fn recovery_accepts_fresh_inventory_generations_with_the_same_configuration() {
        let original =
            PreparedCastDevice::from_capabilities(device(), capabilities(), &resolver(), true)
                .unwrap();
        let mut refreshed_device = device();
        refreshed_device.display_name = "Den TV".into();
        refreshed_device.connection_generation = 9;
        refreshed_device.discovery_generation = 10;
        refreshed_device.device_revision = 11;
        let replacement = PreparedCastDevice::from_capabilities(
            refreshed_device,
            capabilities(),
            &resolver(),
            true,
        )
        .unwrap();

        original.validate_recovery(&replacement).unwrap();
    }

    #[test]
    fn recovery_rejects_identity_and_selected_profile_changes() {
        let original =
            PreparedCastDevice::from_capabilities(device(), capabilities(), &resolver(), true)
                .unwrap();

        let mut changed_identity = capabilities();
        changed_identity.display_identity.product_name = Some("Different TV".into());
        let changed_identity =
            PreparedCastDevice::from_capabilities(device(), changed_identity, &resolver(), true)
                .unwrap();
        assert_eq!(
            original.validate_recovery(&changed_identity),
            Err(PreparedDeviceRecoveryError::EdidChanged)
        );

        let mut changed_profile = capabilities();
        changed_profile.video_profiles[0].max_width = 1280;
        let changed_profile =
            PreparedCastDevice::from_capabilities(device(), changed_profile, &resolver(), true)
                .unwrap();
        assert_eq!(
            original.validate_recovery(&changed_profile),
            Err(PreparedDeviceRecoveryError::VideoProfileChanged(
                "h264-high".into()
            ))
        );
    }
}
