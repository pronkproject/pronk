use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use zvariant::{OwnedFd, OwnedObjectPath, Type};

use super::{
    validate_count, validate_generation, validate_text, validate_token, Validate, ValidationError,
    BACKEND_SESSION_PATH_PREFIX, MAX_AUDIO_PROFILES, MAX_ENDPOINTS, MAX_ERROR_TEXT_BYTES,
    MAX_MANUFACTURER_NAME_BYTES, MAX_MODES, MAX_NODE_NAME_BYTES, MAX_PRODUCT_NAME_BYTES,
    MAX_VIDEO_PROFILES,
};

pub const SESSION_FEATURE_AUDIO: u64 = 1 << 0;
pub const SESSION_FEATURE_CONTROL: u64 = 1 << 1;
pub const KNOWN_SESSION_FEATURES: u64 = SESSION_FEATURE_AUDIO | SESSION_FEATURE_CONTROL;

pub const ERROR_INCOMPATIBLE_PROTOCOL: &str =
    "io.github.pronkproject.Pronk.Error.IncompatibleProtocol";
pub const ERROR_DEVICE_UNAVAILABLE: &str = "io.github.pronkproject.Pronk.Error.DeviceUnavailable";
pub const ERROR_NEGOTIATION_FAILED: &str = "io.github.pronkproject.Pronk.Error.NegotiationFailed";
pub const ERROR_INVALID_PIPEWIRE_REMOTE: &str =
    "io.github.pronkproject.Pronk.Error.InvalidPipeWireRemote";
pub const ERROR_INVALID_MEDIA_TARGET: &str =
    "io.github.pronkproject.Pronk.Error.InvalidMediaTarget";
pub const ERROR_STALE_GENERATION: &str = "io.github.pronkproject.Pronk.Error.StaleGeneration";
pub const ERROR_TRANSPORT_FAILED: &str = "io.github.pronkproject.Pronk.Error.TransportFailed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SessionOptions {
    pub connection_generation: u64,
    pub discovery_generation: u64,
    pub session_generation: u64,
    pub requested_features: u64,
}

impl Validate for SessionOptions {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_generation("connection", self.connection_generation)?;
        validate_generation("discovery", self.discovery_generation)?;
        validate_generation("session", self.session_generation)?;
        validate_feature_bits(self.requested_features)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub flags: u32,
}

impl Validate for DisplayMode {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_range("mode width", self.width as u64, 320, 7_680)?;
        validate_range("mode height", self.height as u64, 240, 4_320)?;
        validate_range(
            "mode refresh millihertz",
            self.refresh_millihz as u64,
            1_000,
            240_000,
        )?;
        if self.flags != 0 {
            return Err(ValidationError::OutOfRange {
                field: "mode flags",
                actual: self.flags as u64,
                minimum: 0,
                maximum: 0,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct VideoProfile {
    pub profile_id: String,
    pub codec: String,
    pub max_width: u32,
    pub max_height: u32,
    pub max_refresh_millihz: u32,
}

impl Validate for VideoProfile {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_profile_id("video profile ID", &self.profile_id)?;
        validate_token("video codec", &self.codec, 64)?;
        validate_range("video maximum width", self.max_width as u64, 320, 7_680)?;
        validate_range("video maximum height", self.max_height as u64, 240, 4_320)?;
        validate_range(
            "video maximum refresh millihertz",
            self.max_refresh_millihz as u64,
            1_000,
            240_000,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct AudioProfile {
    pub profile_id: String,
    pub codec: String,
    pub max_channels: u8,
    pub sample_rates: Vec<u32>,
}

impl Validate for AudioProfile {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_profile_id("audio profile ID", &self.profile_id)?;
        validate_token("audio codec", &self.codec, 64)?;
        validate_range("audio maximum channels", self.max_channels as u64, 1, 8)?;
        validate_count("audio sample rates", self.sample_rates.len(), 16)?;
        if self.sample_rates.is_empty() {
            return Err(ValidationError::Empty {
                field: "audio sample rates",
            });
        }
        let mut rates = HashSet::with_capacity(self.sample_rates.len());
        for rate in &self.sample_rates {
            validate_range("audio sample rate", *rate as u64, 8_000, 192_000)?;
            if !rates.insert(*rate) {
                return Err(ValidationError::DuplicateIdentifier {
                    field: "audio sample rate",
                    value: rate.to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PreparationRequest {
    pub preparation_generation: u64,
    pub candidate_modes: Vec<DisplayMode>,
    pub video_profiles: Vec<VideoProfile>,
    pub audio_profiles: Vec<AudioProfile>,
    pub requested_features: u64,
}

impl Validate for PreparationRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_generation("preparation", self.preparation_generation)?;
        validate_nonempty_bounded("candidate modes", &self.candidate_modes, MAX_MODES)?;
        validate_nonempty_bounded("video profiles", &self.video_profiles, MAX_VIDEO_PROFILES)?;
        validate_bounded("audio profiles", &self.audio_profiles, MAX_AUDIO_PROFILES)?;
        validate_unique_profiles("video profile", &self.video_profiles, |profile| {
            profile.profile_id.as_str()
        })?;
        validate_unique_profiles("audio profile", &self.audio_profiles, |profile| {
            profile.profile_id.as_str()
        })?;
        validate_feature_bits(self.requested_features)?;
        if self.requested_features & SESSION_FEATURE_AUDIO != 0 && self.audio_profiles.is_empty() {
            return Err(ValidationError::InvalidMediaLayout(
                "audio was requested without an audio profile",
            ));
        }
        Ok(())
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum IdentitySource {
    Absent = 0,
    SetupEndpoint = 1,
    AuthenticatedDeviceInfo = 2,
    DiscoveryAdvertisement = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DisplayIdentity {
    pub manufacturer_name: Option<String>,
    pub manufacturer_source: IdentitySource,
    pub product_name: Option<String>,
    pub product_source: IdentitySource,
    pub pnp_id: Option<String>,
}

impl Validate for DisplayIdentity {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_sourced_identity(
            "manufacturer name",
            self.manufacturer_name.as_deref(),
            self.manufacturer_source,
            MAX_MANUFACTURER_NAME_BYTES,
        )?;
        validate_sourced_identity(
            "product name",
            self.product_name.as_deref(),
            self.product_source,
            MAX_PRODUCT_NAME_BYTES,
        )?;
        if let Some(pnp_id) = &self.pnp_id {
            if pnp_id.len() != 3 || !pnp_id.bytes().all(|byte| byte.is_ascii_uppercase()) {
                return Err(ValidationError::InvalidPnpId);
            }
            if !matches!(
                self.manufacturer_source,
                IdentitySource::SetupEndpoint | IdentitySource::AuthenticatedDeviceInfo
            ) {
                return Err(ValidationError::InvalidMediaLayout(
                    "PNP ID requires setup or authenticated device manufacturer provenance",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceCapabilities {
    pub preparation_generation: u64,
    pub display_identity: DisplayIdentity,
    pub modes: Vec<DisplayMode>,
    pub video_profiles: Vec<VideoProfile>,
    pub audio_profiles: Vec<AudioProfile>,
    pub features: u64,
}

impl Validate for DeviceCapabilities {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_generation("preparation", self.preparation_generation)?;
        self.display_identity.validate()?;
        validate_nonempty_bounded("device modes", &self.modes, MAX_MODES)?;
        validate_nonempty_bounded(
            "device video profiles",
            &self.video_profiles,
            MAX_VIDEO_PROFILES,
        )?;
        validate_bounded(
            "device audio profiles",
            &self.audio_profiles,
            MAX_AUDIO_PROFILES,
        )?;
        validate_unique_profiles("video profile", &self.video_profiles, |profile| {
            profile.profile_id.as_str()
        })?;
        validate_unique_profiles("audio profile", &self.audio_profiles, |profile| {
            profile.profile_id.as_str()
        })?;
        validate_feature_bits(self.features)?;
        if self.features & SESSION_FEATURE_AUDIO != 0 && self.audio_profiles.is_empty() {
            return Err(ValidationError::InvalidMediaLayout(
                "audio capability has no audio profile",
            ));
        }
        if self.features & SESSION_FEATURE_AUDIO == 0 && !self.audio_profiles.is_empty() {
            return Err(ValidationError::InvalidMediaLayout(
                "audio profiles require the audio capability",
            ));
        }
        Ok(())
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum MediaKind {
    Video = 1,
    Audio = 2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PipeWireTarget {
    pub kind: MediaKind,
    pub node_name: String,
    pub object_serial: u64,
    pub session_id: String,
    pub device_instance: String,
    pub connector_id: u32,
    pub output_index: u32,
    pub media_generation: u64,
    pub caps: String,
}

impl Validate for PipeWireTarget {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_text("PipeWire node name", &self.node_name, MAX_NODE_NAME_BYTES)?;
        validate_generation("PipeWire object serial", self.object_serial)?;
        validate_session_id(&self.session_id)?;
        validate_token("device instance", &self.device_instance, 128)?;
        validate_range("connector ID", self.connector_id as u64, 1, u32::MAX as u64)?;
        validate_range("output index", self.output_index as u64, 0, 127)?;
        validate_generation("media", self.media_generation)?;
        validate_text("PipeWire caps", &self.caps, MAX_NODE_NAME_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MediaConfiguration {
    pub video_profile_id: String,
    pub audio_profile_id: Option<String>,
    pub mode: DisplayMode,
    pub video_bitrate: u64,
}

impl Validate for MediaConfiguration {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_profile_id("video profile ID", &self.video_profile_id)?;
        if let Some(audio_profile_id) = &self.audio_profile_id {
            validate_profile_id("audio profile ID", audio_profile_id)?;
        }
        self.mode.validate()?;
        validate_range("video bitrate", self.video_bitrate, 1, 1_000_000_000)
    }
}

pub fn validate_media_configuration(
    remote_count: usize,
    targets: &[PipeWireTarget],
    configuration: &MediaConfiguration,
    media_generation: u64,
) -> Result<(), ValidationError> {
    validate_generation("media", media_generation)?;
    configuration.validate()?;
    validate_count("PipeWire targets", targets.len(), MAX_ENDPOINTS)?;
    if remote_count != targets.len() {
        return Err(ValidationError::InvalidMediaLayout(
            "remote count differs from target count",
        ));
    }
    let expected = if configuration.audio_profile_id.is_some() {
        2
    } else {
        1
    };
    if remote_count != expected {
        return Err(ValidationError::InvalidMediaLayout(
            "version 1 requires video, then optional audio",
        ));
    }
    for target in targets {
        target.validate()?;
        if target.media_generation != media_generation {
            return Err(ValidationError::InvalidMediaLayout(
                "target generation differs from ConfigureMedia generation",
            ));
        }
    }
    if targets.first().map(|target| target.kind) != Some(MediaKind::Video) {
        return Err(ValidationError::InvalidMediaLayout(
            "first remote must target video",
        ));
    }
    if expected == 2 && targets.get(1).map(|target| target.kind) != Some(MediaKind::Audio) {
        return Err(ValidationError::InvalidMediaLayout(
            "second remote must target audio",
        ));
    }
    Ok(())
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum SuspendReason {
    OutputDisabled = 1,
    ModeChange = 2,
    DeviceUnavailable = 3,
    SessionInactive = 4,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum StopReason {
    UserRequest = 1,
    DisplayRemoved = 2,
    BackendShutdown = 3,
    TransportFailure = 4,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum SessionState {
    Created = 1,
    Preparing = 2,
    Prepared = 3,
    Configured = 4,
    Streaming = 5,
    Suspended = 6,
    Stopped = 7,
    Failed = 8,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum ControlKind {
    Activate = 1,
    Deactivate = 2,
    Power = 3,
    Standby = 4,
    KeyDown = 5,
    KeyUp = 6,
    Volume = 7,
    Mute = 8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ControlOperation {
    pub session_generation: u64,
    pub kind: ControlKind,
    pub code: Option<String>,
    pub value: i32,
}

impl Validate for ControlOperation {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_generation("session", self.session_generation)?;
        if let Some(code) = &self.code {
            validate_token("control code", code, 64)?;
        }
        let code = self.code.as_deref();
        match self.kind {
            ControlKind::Activate | ControlKind::Deactivate | ControlKind::Standby => {
                require_control_shape(code.is_none() && self.value == 0, "simple operation shape")
            }
            ControlKind::Power => require_control_shape(
                matches!(code, Some("on" | "toggle")) && self.value == 0,
                "power requires code on or toggle and zero value",
            ),
            ControlKind::KeyDown | ControlKind::KeyUp => require_control_shape(
                code.is_some() && self.value == 0,
                "key operation requires code and zero value",
            ),
            ControlKind::Volume => match code {
                Some("relative") => require_control_shape(
                    (-100..=100).contains(&self.value) && self.value != 0,
                    "relative volume must be nonzero in -100..=100",
                ),
                Some("absolute") => require_control_shape(
                    (0..=100).contains(&self.value),
                    "absolute volume must be in 0..=100",
                ),
                _ => Err(ValidationError::InvalidControlOperation(
                    "volume requires relative or absolute code",
                )),
            },
            ControlKind::Mute => require_control_shape(
                matches!(code, Some("on" | "off" | "toggle")) && self.value == 0,
                "mute requires code on, off, or toggle and zero value",
            ),
        }
    }
}

fn require_control_shape(valid: bool, reason: &'static str) -> Result<(), ValidationError> {
    if valid {
        Ok(())
    } else {
        Err(ValidationError::InvalidControlOperation(reason))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SessionStatistics {
    pub session_generation: u64,
    pub media_generation: u64,
    pub video_bitrate: u64,
    pub encoded_frames: u64,
    pub dropped_frames: u64,
    pub queue_delay_micros: u64,
}

impl Validate for SessionStatistics {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_generation("session", self.session_generation)?;
        validate_generation("media", self.media_generation)?;
        validate_range("video bitrate", self.video_bitrate, 0, 1_000_000_000)?;
        validate_range("queue delay", self.queue_delay_micros, 0, 60_000_000)
    }
}

#[zbus::proxy(
    interface = "io.github.pronkproject.Pronk.BackendSession1",
    default_service = "io.github.pronkproject.Pronk.Peer",
    gen_blocking = false
)]
pub trait BackendSession1 {
    fn prepare(&self, request: PreparationRequest) -> zbus::Result<DeviceCapabilities>;
    fn configure_media(
        &self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: u64,
    ) -> zbus::Result<()>;
    fn start(&self, media_generation: u64) -> zbus::Result<()>;
    fn suspend(&self, reason: SuspendReason) -> zbus::Result<()>;
    fn resume(&self, media_generation: u64) -> zbus::Result<()>;
    fn stop_media(&self, media_generation: u64, reason: StopReason) -> zbus::Result<()>;
    fn stop(&self, reason: StopReason) -> zbus::Result<()>;
    fn transmit_control(&self, operation: ControlOperation) -> zbus::Result<u64>;
    fn get_statistics(&self) -> zbus::Result<SessionStatistics>;

    #[zbus(signal)]
    fn state_changed(
        &self,
        session_generation: u64,
        media_generation: u64,
        state: SessionState,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn disconnected(&self, session_generation: u64, error_text: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn keyframe_requested(
        &self,
        session_generation: u64,
        media_generation: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn bitrate_requested(
        &self,
        session_generation: u64,
        media_generation: u64,
        bitrate: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn control_completed(
        &self,
        session_generation: u64,
        operation_id: u64,
        succeeded: bool,
        error_text: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn fatal_error(&self, session_generation: u64, error_text: String) -> zbus::Result<()>;
}

pub fn session_object_path(
    session_id: &str,
    session_generation: u64,
) -> Result<OwnedObjectPath, ValidationError> {
    validate_session_id(session_id)?;
    if session_generation == 0 {
        return Err(ValidationError::ZeroGeneration { field: "session" });
    }
    let encoded: String = session_id
        .chars()
        .filter(|character| *character != '-')
        .collect();
    let path = format!("{BACKEND_SESSION_PATH_PREFIX}s_{encoded}_g{session_generation:016x}");
    OwnedObjectPath::try_from(path).map_err(|_| ValidationError::InvalidSessionPath)
}

pub fn validate_session_object_path(path: &OwnedObjectPath) -> Result<(), ValidationError> {
    let Some(element) = path.as_str().strip_prefix(BACKEND_SESSION_PATH_PREFIX) else {
        return Err(ValidationError::InvalidSessionPath);
    };
    if element.len() != 52
        || !element.starts_with("s_")
        || &element[34..36] != "_g"
        || !element[2..34]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !element[36..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || element[36..].bytes().all(|byte| byte == b'0')
    {
        return Err(ValidationError::InvalidSessionPath);
    }
    Ok(())
}

pub fn validate_error_text(error_text: &str) -> Result<(), ValidationError> {
    validate_text("backend error text", error_text, MAX_ERROR_TEXT_BYTES)
}

fn validate_session_id(session_id: &str) -> Result<(), ValidationError> {
    if session_id.len() != 36 {
        return Err(ValidationError::InvalidSessionId);
    }
    for (index, byte) in session_id.bytes().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return Err(ValidationError::InvalidSessionId);
            }
        } else if !(byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
            return Err(ValidationError::InvalidSessionId);
        }
    }
    Ok(())
}

fn validate_feature_bits(features: u64) -> Result<(), ValidationError> {
    if features & !KNOWN_SESSION_FEATURES != 0 {
        return Err(ValidationError::UnknownCapabilities(
            features & !KNOWN_SESSION_FEATURES,
        ));
    }
    Ok(())
}

fn validate_profile_id(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_token(field, value, 128)
}

fn validate_range(
    field: &'static str,
    actual: u64,
    minimum: u64,
    maximum: u64,
) -> Result<(), ValidationError> {
    if !(minimum..=maximum).contains(&actual) {
        return Err(ValidationError::OutOfRange {
            field,
            actual,
            minimum,
            maximum,
        });
    }
    Ok(())
}

fn validate_sourced_identity(
    field: &'static str,
    value: Option<&str>,
    source: IdentitySource,
    limit: usize,
) -> Result<(), ValidationError> {
    match (value, source) {
        (None, IdentitySource::Absent) => Ok(()),
        (Some(_), IdentitySource::Absent) => {
            Err(ValidationError::UnexpectedIdentityValue { field })
        }
        (None, _) => Err(ValidationError::MissingIdentityValue { field }),
        (Some(value), _) => validate_text(field, value, limit),
    }
}

fn validate_bounded<T: Validate>(
    field: &'static str,
    values: &[T],
    maximum: usize,
) -> Result<(), ValidationError> {
    validate_count(field, values.len(), maximum)?;
    for value in values {
        value.validate()?;
    }
    Ok(())
}

fn validate_nonempty_bounded<T: Validate>(
    field: &'static str,
    values: &[T],
    maximum: usize,
) -> Result<(), ValidationError> {
    if values.is_empty() {
        return Err(ValidationError::Empty { field });
    }
    validate_bounded(field, values, maximum)
}

fn validate_unique_profiles<T, F>(
    field: &'static str,
    values: &[T],
    identifier: F,
) -> Result<(), ValidationError>
where
    F: Fn(&T) -> &str,
{
    let mut identifiers = HashSet::with_capacity(values.len());
    for value in values {
        let identifier = identifier(value);
        if !identifiers.insert(identifier) {
            return Err(ValidationError::DuplicateIdentifier {
                field,
                value: identifier.into(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    fn mode() -> DisplayMode {
        DisplayMode {
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            flags: 0,
        }
    }

    fn video_profile() -> VideoProfile {
        VideoProfile {
            profile_id: "h264-high".into(),
            codec: "h264".into(),
            max_width: 3840,
            max_height: 2160,
            max_refresh_millihz: 60_000,
        }
    }

    fn audio_profile() -> AudioProfile {
        AudioProfile {
            profile_id: "opus-stereo".into(),
            codec: "opus".into(),
            max_channels: 2,
            sample_rates: vec![48_000],
        }
    }

    fn target(kind: MediaKind, generation: u64) -> PipeWireTarget {
        PipeWireTarget {
            kind,
            node_name: match kind {
                MediaKind::Video => "pronk.video.session".into(),
                MediaKind::Audio => "pronk.audio.session".into(),
            },
            object_serial: 12,
            session_id: SESSION_ID.into(),
            device_instance: "castkms-card1".into(),
            connector_id: 51,
            output_index: 0,
            media_generation: generation,
            caps: "video/x-raw,format=BGRx".into(),
        }
    }

    #[test]
    fn validates_preparation_bounds_and_identity_provenance() {
        let request = PreparationRequest {
            preparation_generation: 1,
            candidate_modes: vec![mode()],
            video_profiles: vec![video_profile()],
            audio_profiles: vec![audio_profile()],
            requested_features: SESSION_FEATURE_AUDIO,
        };
        request.validate().unwrap();

        let identity = DisplayIdentity {
            manufacturer_name: Some("Sony".into()),
            manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
            product_name: Some("Living Room Television".into()),
            product_source: IdentitySource::SetupEndpoint,
            pnp_id: Some("SON".into()),
        };
        identity.validate().unwrap();

        let invalid = DisplayIdentity {
            pnp_id: Some("Sony".into()),
            ..identity
        };
        assert_eq!(invalid.validate(), Err(ValidationError::InvalidPnpId));
    }

    #[test]
    fn enforces_video_then_optional_audio_remote_layout() {
        let configuration = MediaConfiguration {
            video_profile_id: "h264-high".into(),
            audio_profile_id: Some("opus-stereo".into()),
            mode: mode(),
            video_bitrate: 8_000_000,
        };
        validate_media_configuration(
            2,
            &[target(MediaKind::Video, 7), target(MediaKind::Audio, 7)],
            &configuration,
            7,
        )
        .unwrap();

        assert!(matches!(
            validate_media_configuration(
                2,
                &[target(MediaKind::Audio, 7), target(MediaKind::Video, 7)],
                &configuration,
                7,
            ),
            Err(ValidationError::InvalidMediaLayout(_))
        ));
        assert!(matches!(
            validate_media_configuration(1, &[target(MediaKind::Video, 8)], &configuration, 7,),
            Err(ValidationError::InvalidMediaLayout(_))
        ));
    }

    #[test]
    fn session_generation_has_one_canonical_object_path() {
        let path = session_object_path(SESSION_ID, 1).unwrap();
        assert_eq!(
            path.as_str(),
            "/io/github/pronkproject/Pronk/Backend/Sessions/s_0123456789abcdef0123456789abcdef_g0000000000000001"
        );
        validate_session_object_path(&path).unwrap();
        assert_eq!(
            session_object_path("01234567-89AB-cdef-0123-456789abcdef", 1),
            Err(ValidationError::InvalidSessionId)
        );
        assert_eq!(
            session_object_path(SESSION_ID, 0),
            Err(ValidationError::ZeroGeneration { field: "session" })
        );
        assert_ne!(
            session_object_path(SESSION_ID, 1).unwrap(),
            session_object_path(SESSION_ID, 2).unwrap()
        );
    }

    #[test]
    fn validates_normalized_control_operation_shapes() {
        for operation in [
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Activate,
                code: None,
                value: 0,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Power,
                code: Some("on".into()),
                value: 0,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::KeyDown,
                code: Some("cec-ui-44".into()),
                value: 0,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Volume,
                code: Some("relative".into()),
                value: -5,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Volume,
                code: Some("absolute".into()),
                value: 100,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Mute,
                code: Some("toggle".into()),
                value: 0,
            },
        ] {
            operation.validate().unwrap();
        }

        for operation in [
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Activate,
                code: Some("unexpected".into()),
                value: 0,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Volume,
                code: Some("relative".into()),
                value: 0,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Volume,
                code: Some("absolute".into()),
                value: 101,
            },
            ControlOperation {
                session_generation: 1,
                kind: ControlKind::Mute,
                code: None,
                value: 0,
            },
        ] {
            assert!(matches!(
                operation.validate(),
                Err(ValidationError::InvalidControlOperation(_))
            ));
        }
    }

    #[test]
    fn session_wire_signatures_are_stable() {
        assert_eq!(SessionOptions::SIGNATURE, "(tttt)");
        assert_eq!(DisplayMode::SIGNATURE, "(uuuu)");
        assert_eq!(VideoProfile::SIGNATURE, "(ssuuu)");
        assert_eq!(AudioProfile::SIGNATURE, "(ssyau)");
        assert_eq!(IdentitySource::SIGNATURE, "u");
        assert_eq!(IdentitySource::SetupEndpoint as u32, 1);
        assert_eq!(DisplayIdentity::SIGNATURE, "(asuasuas)");
        assert_eq!(MediaKind::SIGNATURE, "u");
        assert_eq!(ControlKind::SIGNATURE, "u");
        assert_eq!(ControlOperation::SIGNATURE, "(tuasi)");
        assert_eq!(OwnedFd::SIGNATURE, "h");
    }
}
