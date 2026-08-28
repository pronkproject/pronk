//! Typed private protocol shared by Pronk and device backends.
//!
//! Every value received from the wire must be validated before it is admitted
//! to core state. The D-Bus signatures provide types, while [`Validate`] adds
//! the protocol's semantic and resource bounds.

use std::collections::HashSet;

use nix::unistd::Uid;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use thiserror::Error;
use tokio::net::UnixStream;
use zbus::connection::socket::BoxedSplit;
use zbus::connection::{AuthMechanism, Builder};
use zbus::{Connection, Guid};
use zvariant::Type;

mod session;

pub use session::*;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;

pub const CAPABILITY_PIPEWIRE_REMOTE_FDS_V1: u64 = 1 << 0;
pub const KNOWN_CAPABILITIES: u64 = CAPABILITY_PIPEWIRE_REMOTE_FDS_V1;
pub const REQUIRED_CAPABILITIES: u64 = CAPABILITY_PIPEWIRE_REMOTE_FDS_V1;

pub const BACKEND_HOST_INTERFACE: &str = "io.github.pronkproject.Pronk.BackendHost1";
pub const BACKEND_INTERFACE: &str = "io.github.pronkproject.Pronk.Backend1";
pub const BACKEND_SESSION_INTERFACE: &str = "io.github.pronkproject.Pronk.BackendSession1";
pub const BACKEND_HOST_PATH: &str = "/io/github/pronkproject/Pronk/BackendHost";
pub const BACKEND_PATH: &str = "/io/github/pronkproject/Pronk/Backend";
pub const BACKEND_SESSION_PATH_PREFIX: &str = "/io/github/pronkproject/Pronk/Backend/Sessions/";

// Generated zbus proxies require a syntactically valid destination, including
// on P2P connections. It is only a fixed header value: no peer owns or routes
// this name.
pub const P2P_DESTINATION: &str = "io.github.pronkproject.Pronk.Peer";

pub const MAX_BACKEND_ID_BYTES: usize = 64;
pub const MAX_DEVICE_TEXT_BYTES: usize = 256;
pub const MAX_DISPLAY_NAME_BYTES: usize = 256;
pub const MAX_BUILD_VERSION_BYTES: usize = 128;
pub const MAX_ACTIVATION_ID_BYTES: usize = 128;
pub const MAX_DISCOVERY_METADATA_ENTRIES: usize = 16;
pub const MAX_DISCOVERY_METADATA_KEY_BYTES: usize = 64;
pub const MAX_DISCOVERY_METADATA_VALUE_BYTES: usize = 256;
pub const MAX_DEVICES: usize = 128;
pub const MAX_ERROR_TEXT_BYTES: usize = 512;
pub const MAX_MODES: usize = 64;
pub const MAX_VIDEO_PROFILES: usize = 16;
pub const MAX_AUDIO_PROFILES: usize = 16;
pub const MAX_ENDPOINTS: usize = 16;
pub const MAX_NODE_NAME_BYTES: usize = 256;
pub const MAX_MANUFACTURER_NAME_BYTES: usize = 128;
pub const MAX_PRODUCT_NAME_BYTES: usize = 106;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct BackendInfo {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub capabilities: u64,
    pub required_capabilities: u64,
    pub backend_id: String,
    pub display_name: String,
    pub build_version: String,
    pub activation_instance: String,
    pub invocation_id: String,
}

impl BackendInfo {
    pub fn v1(
        backend_id: impl Into<String>,
        display_name: impl Into<String>,
        build_version: impl Into<String>,
        activation_instance: impl Into<String>,
        invocation_id: impl Into<String>,
    ) -> Self {
        Self {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: KNOWN_CAPABILITIES,
            required_capabilities: REQUIRED_CAPABILITIES,
            backend_id: backend_id.into(),
            display_name: display_name.into(),
            build_version: build_version.into(),
            activation_instance: activation_instance.into(),
            invocation_id: invocation_id.into(),
        }
    }
}

impl Validate for BackendInfo {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol_major != PROTOCOL_MAJOR {
            return Err(ValidationError::IncompatibleMajor(self.protocol_major));
        }
        if self.capabilities & REQUIRED_CAPABILITIES != REQUIRED_CAPABILITIES {
            return Err(ValidationError::MissingCapabilities(
                REQUIRED_CAPABILITIES & !self.capabilities,
            ));
        }
        if self.required_capabilities & !KNOWN_CAPABILITIES != 0 {
            return Err(ValidationError::UnknownCapabilities(
                self.required_capabilities & !KNOWN_CAPABILITIES,
            ));
        }
        if self.required_capabilities & !self.capabilities != 0 {
            return Err(ValidationError::RequiredCapabilitiesNotAdvertised(
                self.required_capabilities & !self.capabilities,
            ));
        }
        validate_backend_id(&self.backend_id)?;
        validate_text(
            "backend display name",
            &self.display_name,
            MAX_DISPLAY_NAME_BYTES,
        )?;
        validate_text(
            "build version",
            &self.build_version,
            MAX_BUILD_VERSION_BYTES,
        )?;
        validate_token(
            "activation instance",
            &self.activation_instance,
            MAX_ACTIVATION_ID_BYTES,
        )?;
        validate_token(
            "invocation ID",
            &self.invocation_id,
            MAX_ACTIVATION_ID_BYTES,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RegistrationReply {
    pub protocol_minor: u16,
    pub connection_generation: u64,
}

impl Validate for RegistrationReply {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol_minor > PROTOCOL_MINOR {
            return Err(ValidationError::IncompatibleMinor(self.protocol_minor));
        }
        validate_generation("connection", self.connection_generation)
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum DeviceAvailability {
    Available = 1,
    Busy = 2,
    Unavailable = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DiscoveryMetadataEntry {
    pub key: String,
    pub value: String,
}

impl Validate for DiscoveryMetadataEntry {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_metadata_key(&self.key)?;
        validate_text(
            "discovery metadata value",
            &self.value,
            MAX_DISCOVERY_METADATA_VALUE_BYTES,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceInfo {
    pub backend_id: String,
    pub device_id: String,
    pub display_name: String,
    pub availability: DeviceAvailability,
    pub metadata: Vec<DiscoveryMetadataEntry>,
}

impl Validate for DeviceInfo {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_backend_id(&self.backend_id)?;
        validate_text("device ID", &self.device_id, MAX_DEVICE_TEXT_BYTES)?;
        validate_text(
            "device display name",
            &self.display_name,
            MAX_DEVICE_TEXT_BYTES,
        )?;
        validate_count(
            "discovery metadata",
            self.metadata.len(),
            MAX_DISCOVERY_METADATA_ENTRIES,
        )?;

        let mut keys = HashSet::with_capacity(self.metadata.len());
        for entry in &self.metadata {
            entry.validate()?;
            if !keys.insert(entry.key.as_str()) {
                return Err(ValidationError::DuplicateMetadataKey(entry.key.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceSnapshot {
    pub discovery_generation: u64,
    pub revision: u64,
    pub devices: Vec<DeviceInfo>,
}

impl Validate for DeviceSnapshot {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_generation("discovery", self.discovery_generation)?;
        validate_count("devices", self.devices.len(), MAX_DEVICES)?;

        let mut identities = HashSet::with_capacity(self.devices.len());
        for device in &self.devices {
            device.validate()?;
            if !identities.insert((device.backend_id.as_str(), device.device_id.as_str())) {
                return Err(ValidationError::DuplicateDevice {
                    backend_id: device.backend_id.clone(),
                    device_id: device.device_id.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceIdentity {
    pub backend_id: String,
    pub device_id: String,
}

impl Validate for DeviceIdentity {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_backend_id(&self.backend_id)?;
        validate_text("device ID", &self.device_id, MAX_DEVICE_TEXT_BYTES)
    }
}

#[zbus::proxy(
    interface = "io.github.pronkproject.Pronk.BackendHost1",
    default_service = "io.github.pronkproject.Pronk.Peer",
    default_path = "/io/github/pronkproject/Pronk/BackendHost",
    gen_blocking = false
)]
pub trait BackendHost1 {
    fn register_backend(&self, info: BackendInfo) -> zbus::Result<RegistrationReply>;
}

#[zbus::proxy(
    interface = "io.github.pronkproject.Pronk.Backend1",
    default_service = "io.github.pronkproject.Pronk.Peer",
    default_path = "/io/github/pronkproject/Pronk/Backend",
    gen_blocking = false
)]
pub trait Backend1 {
    fn get_info(&self) -> zbus::Result<BackendInfo>;
    fn start_discovery(&self) -> zbus::Result<u64>;
    fn stop_discovery(&self, discovery_generation: u64) -> zbus::Result<()>;
    fn list_devices(&self) -> zbus::Result<DeviceSnapshot>;
    fn create_session(
        &self,
        session_id: String,
        device_id: String,
        options: SessionOptions,
    ) -> zbus::Result<zvariant::OwnedObjectPath>;
    fn shutdown(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn device_added(
        &self,
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn device_changed(
        &self,
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn device_removed(
        &self,
        discovery_generation: u64,
        revision: u64,
        device: DeviceIdentity,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn fatal_error(&self, connection_generation: u64, error_text: String) -> zbus::Result<()>;
}

/// Construct the Pronk/server side of a private P2P D-Bus connection.
pub fn backend_host_builder<S>(socket: S) -> zbus::Result<Builder<'static>>
where
    S: Into<BoxedSplit>,
{
    Ok(Builder::socket(socket)
        .server(Guid::generate())?
        .p2p()
        .auth_mechanism(AuthMechanism::External)
        .max_queued(64))
}

/// Construct the device-backend/client side of a private P2P D-Bus connection.
pub fn backend_peer_builder(stream: UnixStream) -> Builder<'static> {
    Builder::unix_stream(stream)
        .p2p()
        .auth_mechanism(AuthMechanism::External)
        .max_queued(64)
}

/// Require the transport peer to have the current process's effective UID.
///
/// EXTERNAL authenticates the client to the server. This explicit transport
/// check additionally authenticates the server to the client.
pub async fn require_same_uid(connection: &Connection) -> Result<(), PeerIdentityError> {
    let credentials = connection
        .peer_credentials()
        .await
        .map_err(PeerIdentityError::Inspect)?;
    let actual = credentials
        .unix_user_id()
        .ok_or(PeerIdentityError::MissingUnixUserId)?;
    let expected = Uid::effective().as_raw();
    if actual != expected {
        return Err(PeerIdentityError::WrongUnixUserId { expected, actual });
    }
    Ok(())
}

pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("protocol major {0} is incompatible with major {PROTOCOL_MAJOR}")]
    IncompatibleMajor(u16),
    #[error("protocol minor {0} is newer than supported minor {PROTOCOL_MINOR}")]
    IncompatibleMinor(u16),
    #[error("required capabilities 0x{0:x} are missing")]
    MissingCapabilities(u64),
    #[error("unknown required capabilities 0x{0:x}")]
    UnknownCapabilities(u64),
    #[error("required capabilities 0x{0:x} are not advertised as supported")]
    RequiredCapabilitiesNotAdvertised(u64),
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} is {actual} bytes; limit is {limit}")]
    TooLong {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("{field} contains a control character")]
    ControlCharacter { field: &'static str },
    #[error("{field} contains a character outside its wire grammar")]
    InvalidToken { field: &'static str },
    #[error("{field} generation must be nonzero")]
    ZeroGeneration { field: &'static str },
    #[error("{field} has {actual} entries; limit is {limit}")]
    TooMany {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("backend ID contains a character outside [a-z0-9-]")]
    InvalidBackendId,
    #[error("duplicate discovery metadata key {0:?}")]
    DuplicateMetadataKey(String),
    #[error("duplicate device {backend_id:?}/{device_id:?}")]
    DuplicateDevice {
        backend_id: String,
        device_id: String,
    },
    #[error("{field} value {actual} is outside {minimum}..={maximum}")]
    OutOfRange {
        field: &'static str,
        actual: u64,
        minimum: u64,
        maximum: u64,
    },
    #[error("{field} must be absent when its source is Absent")]
    UnexpectedIdentityValue { field: &'static str },
    #[error("{field} must be present when its source is not Absent")]
    MissingIdentityValue { field: &'static str },
    #[error("PNP ID must contain exactly three ASCII uppercase letters")]
    InvalidPnpId,
    #[error("duplicate {field} identifier {value:?}")]
    DuplicateIdentifier { field: &'static str, value: String },
    #[error("session ID is not a canonical lowercase UUID")]
    InvalidSessionId,
    #[error("session object path is outside the BackendSession1 subtree")]
    InvalidSessionPath,
    #[error("media remote/target layout is invalid: {0}")]
    InvalidMediaLayout(&'static str),
    #[error("control operation is invalid: {0}")]
    InvalidControlOperation(&'static str),
}

#[derive(Debug, Error)]
pub enum PeerIdentityError {
    #[error("cannot inspect P2P peer credentials: {0}")]
    Inspect(std::io::Error),
    #[error("P2P peer credentials have no Unix user ID")]
    MissingUnixUserId,
    #[error("P2P peer UID {actual} does not match effective UID {expected}")]
    WrongUnixUserId { expected: u32, actual: u32 },
}

pub(crate) fn validate_backend_id(value: &str) -> Result<(), ValidationError> {
    validate_text("backend ID", value, MAX_BACKEND_ID_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ValidationError::InvalidBackendId);
    }
    Ok(())
}

fn validate_metadata_key(value: &str) -> Result<(), ValidationError> {
    validate_token(
        "discovery metadata key",
        value,
        MAX_DISCOVERY_METADATA_KEY_BYTES,
    )
}

pub(crate) fn validate_token(
    field: &'static str,
    value: &str,
    limit: usize,
) -> Result<(), ValidationError> {
    validate_text(field, value, limit)?;
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
    }) {
        return Err(ValidationError::InvalidToken { field });
    }
    Ok(())
}

pub(crate) fn validate_text(
    field: &'static str,
    value: &str,
    limit: usize,
) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty { field });
    }
    if value.len() > limit {
        return Err(ValidationError::TooLong {
            field,
            actual: value.len(),
            limit,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::ControlCharacter { field });
    }
    Ok(())
}

pub(crate) fn validate_generation(
    field: &'static str,
    generation: u64,
) -> Result<(), ValidationError> {
    if generation == 0 {
        return Err(ValidationError::ZeroGeneration { field });
    }
    Ok(())
}

pub(crate) fn validate_count(
    field: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), ValidationError> {
    if actual > limit {
        return Err(ValidationError::TooMany {
            field,
            actual,
            limit,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> DeviceInfo {
        DeviceInfo {
            backend_id: "mock".into(),
            device_id: id.into(),
            display_name: format!("Device {id}"),
            availability: DeviceAvailability::Available,
            metadata: vec![DiscoveryMetadataEntry {
                key: "model".into(),
                value: "Deterministic Mock".into(),
            }],
        }
    }

    #[test]
    fn validates_v1_backend_info_and_capabilities() {
        let info = BackendInfo::v1("mock", "Mock backend", "0.1.0", "mock", "deadbeef");
        info.validate().unwrap();

        let mut missing = info.clone();
        missing.capabilities = 0;
        assert_eq!(
            missing.validate(),
            Err(ValidationError::MissingCapabilities(
                CAPABILITY_PIPEWIRE_REMOTE_FDS_V1
            ))
        );

        let mut optional_unknown = info.clone();
        optional_unknown.protocol_minor = PROTOCOL_MINOR + 1;
        optional_unknown.capabilities |= 1 << 63;
        optional_unknown.validate().unwrap();

        let mut unknown = info;
        unknown.capabilities |= 1 << 63;
        unknown.required_capabilities |= 1 << 63;
        assert_eq!(
            unknown.validate(),
            Err(ValidationError::UnknownCapabilities(1 << 63))
        );
    }

    #[test]
    fn rejects_unbounded_or_ambiguous_device_data() {
        let mut invalid = device("living-room");
        invalid.display_name = "x".repeat(MAX_DEVICE_TEXT_BYTES + 1);
        assert!(matches!(
            invalid.validate(),
            Err(ValidationError::TooLong {
                field: "device display name",
                ..
            })
        ));

        let mut duplicate_metadata = device("living-room");
        duplicate_metadata.metadata.push(DiscoveryMetadataEntry {
            key: "model".into(),
            value: "Other".into(),
        });
        assert!(matches!(
            duplicate_metadata.validate(),
            Err(ValidationError::DuplicateMetadataKey(_))
        ));
    }

    #[test]
    fn validates_snapshot_generation_and_unique_identity() {
        let snapshot = DeviceSnapshot {
            discovery_generation: 1,
            revision: 2,
            devices: vec![device("one"), device("two")],
        };
        snapshot.validate().unwrap();

        let duplicate = DeviceSnapshot {
            devices: vec![device("one"), device("one")],
            ..snapshot
        };
        assert!(matches!(
            duplicate.validate(),
            Err(ValidationError::DuplicateDevice { .. })
        ));
    }

    #[test]
    fn wire_signatures_are_stable() {
        assert_eq!(BackendInfo::SIGNATURE, "(qqttsssss)");
        assert_eq!(RegistrationReply::SIGNATURE, "(qt)");
        assert_eq!(DeviceAvailability::SIGNATURE, "u");
        assert_eq!(DiscoveryMetadataEntry::SIGNATURE, "(ss)");
        assert_eq!(DeviceInfo::SIGNATURE, "(sssua(ss))");
        assert_eq!(DeviceSnapshot::SIGNATURE, "(tta(sssua(ss)))");
    }
}
