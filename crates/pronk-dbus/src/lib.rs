//! Public, non-sensitive Pronk session-bus contract.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use thiserror::Error;
use zvariant::{OwnedObjectPath, Type};

pub const BUS_NAME: &str = "io.github.pronkproject.Pronk1";
pub const MANAGER_INTERFACE: &str = "io.github.pronkproject.Pronk1.Manager";
pub const MANAGER_PATH: &str = "/io/github/pronkproject/Pronk1";
pub const OPERATION_INTERFACE: &str = "io.github.pronkproject.Pronk1.Operation";
pub const OPERATION_PATH_PREFIX: &str = "/io/github/pronkproject/Pronk1/operation";
pub const CAST_DISPLAY_INTERFACE: &str = "io.github.pronkproject.Pronk1.CastDisplay";
pub const CAST_DISPLAY_PATH_PREFIX: &str = "/io/github/pronkproject/Pronk1/display";
pub const MEDIA_SESSION_INTERFACE: &str = "io.github.pronkproject.Pronk1.MediaSession";

pub const API_MAJOR: u16 = 1;
pub const API_MINOR: u16 = 4;
pub const API_FEATURE_DEVICE_INVENTORY: u64 = 1 << 0;
pub const API_FEATURE_CAST_DISPLAY_LIFECYCLE: u64 = 1 << 1;
pub const API_FEATURE_CAST_DISPLAY_STATE: u64 = 1 << 2;
pub const API_FEATURE_CAST_DISPLAY_DYNAMIC_STATE: u64 = 1 << 3;
pub const API_FEATURE_MEDIA_SESSION_STATE: u64 = 1 << 4;
pub const API_FEATURES: u64 = API_FEATURE_DEVICE_INVENTORY
    | API_FEATURE_CAST_DISPLAY_LIFECYCLE
    | API_FEATURE_CAST_DISPLAY_STATE
    | API_FEATURE_CAST_DISPLAY_DYNAMIC_STATE
    | API_FEATURE_MEDIA_SESSION_STATE;

pub const MAX_PUBLIC_DEVICES: usize = 128;
pub const MAX_PUBLIC_DISPLAYS: usize = 64;
pub const MAX_BACKEND_ID_BYTES: usize = 64;
pub const MAX_DEVICE_TEXT_BYTES: usize = 256;
pub const MAX_METADATA_ENTRIES: usize = 16;
pub const MAX_METADATA_KEY_BYTES: usize = 64;
pub const MAX_METADATA_VALUE_BYTES: usize = 256;
pub const MAX_OPERATION_ERROR_BYTES: usize = 512;
pub const MAX_MEDIA_ERROR_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ApiVersion {
    pub major: u16,
    pub minor: u16,
    pub features: u64,
}

impl ApiVersion {
    pub const CURRENT: Self = Self {
        major: API_MAJOR,
        minor: API_MINOR,
        features: API_FEATURES,
    };
}

pub fn cast_display_object_path(display_id: &str) -> Result<OwnedObjectPath, ValidationError> {
    validate_display_id(display_id)?;
    let segment = display_id.replace('-', "");
    OwnedObjectPath::try_from(format!("{CAST_DISPLAY_PATH_PREFIX}/{segment}"))
        .map_err(|_| ValidationError::InvalidDisplayId)
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum DeviceAvailability {
    Available = 1,
    Busy = 2,
    Unavailable = 3,
}

impl std::fmt::Display for DeviceAvailability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Available => "Available",
            Self::Busy => "Busy",
            Self::Unavailable => "Unavailable",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DiscoveryMetadataEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceInfo {
    pub backend_id: String,
    pub device_id: String,
    pub display_name: String,
    pub availability: DeviceAvailability,
    pub connection_generation: u64,
    pub discovery_generation: u64,
    /// Core-owned token advanced only when this device materially changes.
    pub device_revision: u64,
    pub metadata: Vec<DiscoveryMetadataEntry>,
}

impl DeviceInfo {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_backend_id(&self.backend_id)?;
        validate_text("device ID", &self.device_id, MAX_DEVICE_TEXT_BYTES)?;
        validate_text(
            "device display name",
            &self.display_name,
            MAX_DEVICE_TEXT_BYTES,
        )?;
        validate_nonzero("connection", self.connection_generation)?;
        validate_nonzero("discovery", self.discovery_generation)?;
        validate_nonzero("device revision", self.device_revision)?;
        if self.metadata.len() > MAX_METADATA_ENTRIES {
            return Err(ValidationError::TooMany {
                field: "discovery metadata",
                actual: self.metadata.len(),
                limit: MAX_METADATA_ENTRIES,
            });
        }
        let mut keys = HashSet::with_capacity(self.metadata.len());
        for entry in &self.metadata {
            validate_text("metadata key", &entry.key, MAX_METADATA_KEY_BYTES)?;
            validate_text("metadata value", &entry.value, MAX_METADATA_VALUE_BYTES)?;
            if !keys.insert(entry.key.as_str()) {
                return Err(ValidationError::DuplicateMetadataKey(entry.key.clone()));
            }
        }
        Ok(())
    }
}

/// Exact optimistic-concurrency token copied from one displayed device row.
///
/// The global inventory revision is deliberately absent. A change to another
/// device must not invalidate this selection, while any material change to the
/// selected device must.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceSelection {
    pub backend_id: String,
    pub device_id: String,
    pub connection_generation: u64,
    pub discovery_generation: u64,
    pub device_revision: u64,
}

impl DeviceSelection {
    pub fn from_device(device: &DeviceInfo) -> Self {
        Self {
            backend_id: device.backend_id.clone(),
            device_id: device.device_id.clone(),
            connection_generation: device.connection_generation,
            discovery_generation: device.discovery_generation,
            device_revision: device.device_revision,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_backend_id(&self.backend_id)?;
        validate_text("device ID", &self.device_id, MAX_DEVICE_TEXT_BYTES)?;
        validate_nonzero("connection", self.connection_generation)?;
        validate_nonzero("discovery", self.discovery_generation)?;
        validate_nonzero("device revision", self.device_revision)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceSnapshot {
    /// Global public inventory revision. Zero denotes an untouched empty inventory.
    pub inventory_revision: u64,
    pub devices: Vec<DeviceInfo>,
}

impl DeviceSnapshot {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.devices.len() > MAX_PUBLIC_DEVICES {
            return Err(ValidationError::TooMany {
                field: "devices",
                actual: self.devices.len(),
                limit: MAX_PUBLIC_DEVICES,
            });
        }
        let mut identities = HashSet::with_capacity(self.devices.len());
        let mut previous_identity: Option<(&str, &str)> = None;
        for device in &self.devices {
            device.validate()?;
            let identity = (device.backend_id.as_str(), device.device_id.as_str());
            if !identities.insert(identity) {
                return Err(ValidationError::DuplicateDevice {
                    backend_id: device.backend_id.clone(),
                    device_id: device.device_id.clone(),
                });
            }
            if device.device_revision > self.inventory_revision {
                return Err(ValidationError::FutureDeviceRevision {
                    backend_id: device.backend_id.clone(),
                    device_id: device.device_id.clone(),
                    device_revision: device.device_revision,
                    inventory_revision: self.inventory_revision,
                });
            }
            if previous_identity.is_some_and(|previous| previous >= identity) {
                return Err(ValidationError::UnorderedDevices);
            }
            previous_identity = Some(identity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DisplaySetupOptions {
    pub audio_enabled: bool,
}

impl Default for DisplaySetupOptions {
    fn default() -> Self {
        Self {
            audio_enabled: true,
        }
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum OperationStage {
    Validating = 1,
    Authorizing = 2,
    PreparingDevice = 3,
    Attaching = 4,
    Added = 5,
    Cancelled = 6,
    Failed = 7,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
/// Stable, machine-readable terminal result for an AddDisplay operation.
///
/// Diagnostics remain suitable for logs and troubleshooting, but clients
/// should make retry and presentation decisions from this value.
pub enum OperationErrorCode {
    None = 0,
    Cancelled = 1,
    CallerExited = 2,
    InvalidRequest = 3,
    DeviceNotFound = 4,
    DeviceChanged = 5,
    DeviceUnavailable = 6,
    BackendUnavailable = 7,
    CapacityExhausted = 8,
    DeviceAlreadyAdded = 9,
    AuthorizationFailed = 10,
    DevicePreparationFailed = 11,
    AttachmentFailed = 12,
    Internal = 13,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum DisplayIdentitySource {
    Absent = 0,
    SetupEndpoint = 1,
    AuthenticatedDeviceInfo = 2,
    DiscoveryAdvertisement = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum PnpResolutionSource {
    AuthenticatedPnpId = 1,
    ExactName = 2,
    LegalSuffixName = 3,
    ReviewedAlias = 4,
    SynthesizerFallback = 5,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum DisplayAttachmentState {
    Attached = 1,
    Detached = 2,
    Unknown = 3,
}

impl std::fmt::Display for DisplayAttachmentState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Attached => "Attached",
            Self::Detached => "Detached",
            Self::Unknown => "Unknown",
        })
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum DisplayRouteState {
    Disabled = 1,
    Active = 2,
}

impl std::fmt::Display for DisplayRouteState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Disabled => "Disabled",
            Self::Active => "Active",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RoutedDisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub flags: u32,
}

/// Stable, policy-level media phase for one configured cast display.
///
/// Capture setup and backend setup are intentionally coalesced into
/// `Starting`, while backend replacement, PipeWire replacement, and mode
/// reconfiguration are coalesced into `Recovering`. Public clients do not
/// need to understand the implementation phases behind those transitions.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr, Type)]
pub enum MediaSessionPhase {
    Inactive = 1,
    Starting = 2,
    Running = 3,
    Suspended = 4,
    Recovering = 5,
    Stopping = 6,
    Failed = 7,
}

impl std::fmt::Display for MediaSessionPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Inactive => "Inactive",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Suspended => "Suspended",
            Self::Recovering => "Recovering",
            Self::Stopping => "Stopping",
            Self::Failed => "Failed",
        })
    }
}

/// Read-only public media projection on the cast display's object path.
///
/// Subscribe to `MediaSession1.StateChanged` before calling `GetState`, drop
/// queued states at or below the returned revision, and then accept only
/// strictly newer states. The revision shares the cast display's monotonic
/// state domain and may therefore jump when a non-media observation happened
/// between two media transitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MediaSessionState {
    pub revision: u64,
    pub phase: MediaSessionPhase,
    /// Zero before the first media generation. It remains diagnostic after a
    /// generation returns to `Inactive`.
    pub media_generation: u64,
    /// Whether audio was negotiated for this configured cast display.
    pub audio_enabled: bool,
    /// Nonempty only in `Failed`.
    pub error: String,
}

impl MediaSessionState {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_nonzero("media-session state revision", self.revision)?;
        validate_optional_text("media-session error", &self.error, MAX_MEDIA_ERROR_BYTES)?;
        if self.media_generation == 0
            && matches!(
                self.phase,
                MediaSessionPhase::Starting
                    | MediaSessionPhase::Running
                    | MediaSessionPhase::Suspended
            )
        {
            return Err(ValidationError::InvalidMediaGeneration);
        }
        if self.error.is_empty() {
            if self.phase == MediaSessionPhase::Failed {
                return Err(ValidationError::InvalidMediaError);
            }
        } else if self.phase != MediaSessionPhase::Failed {
            return Err(ValidationError::InvalidMediaError);
        }
        Ok(())
    }
}

impl OperationStage {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Added | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct OperationState {
    pub display_id: String,
    pub stage: OperationStage,
    pub error_code: OperationErrorCode,
    /// Empty unless the operation is terminal with diagnostic detail.
    pub error: String,
}

impl OperationState {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_display_id(&self.display_id)?;
        validate_optional_text("operation error", &self.error, MAX_OPERATION_ERROR_BYTES)?;
        let valid_error = match self.stage {
            OperationStage::Cancelled => {
                matches!(
                    self.error_code,
                    OperationErrorCode::Cancelled | OperationErrorCode::CallerExited
                ) && !self.error.is_empty()
            }
            OperationStage::Failed => {
                !matches!(
                    self.error_code,
                    OperationErrorCode::None
                        | OperationErrorCode::Cancelled
                        | OperationErrorCode::CallerExited
                ) && !self.error.is_empty()
            }
            OperationStage::Validating
            | OperationStage::Authorizing
            | OperationStage::PreparingDevice
            | OperationStage::Attaching
            | OperationStage::Added => {
                self.error_code == OperationErrorCode::None && self.error.is_empty()
            }
        };
        if !valid_error {
            return Err(ValidationError::InvalidOperationError);
        }
        Ok(())
    }
}

/// Public projection of one explicitly added cast display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CastDisplayInfo {
    pub display_id: String,
    pub backend_id: String,
    pub device_id: String,
    pub display_name: String,
    /// Empty when the backend did not authenticate a manufacturer name.
    pub manufacturer_name: String,
    pub manufacturer_source: DisplayIdentitySource,
    /// Empty when the backend did not authenticate a product name.
    pub product_name: String,
    pub product_source: DisplayIdentitySource,
    pub pnp_id: String,
    pub pnp_resolution_source: PnpResolutionSource,
    pub connector_id: u32,
    pub connector_name: String,
    pub output_index: u32,
    pub product_code: u16,
    pub serial: u32,
    pub attachment_state: DisplayAttachmentState,
}

impl CastDisplayInfo {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_display_id(&self.display_id)?;
        validate_backend_id(&self.backend_id)?;
        validate_text("device ID", &self.device_id, MAX_DEVICE_TEXT_BYTES)?;
        validate_text("display name", &self.display_name, MAX_DEVICE_TEXT_BYTES)?;
        validate_optional_text(
            "manufacturer name",
            &self.manufacturer_name,
            MAX_DEVICE_TEXT_BYTES,
        )?;
        validate_identity_source(
            "manufacturer name",
            &self.manufacturer_name,
            self.manufacturer_source,
        )?;
        validate_optional_text("product name", &self.product_name, MAX_DEVICE_TEXT_BYTES)?;
        validate_identity_source("product name", &self.product_name, self.product_source)?;
        if self.pnp_id.len() != 3 || !self.pnp_id.bytes().all(|byte| byte.is_ascii_uppercase()) {
            return Err(ValidationError::InvalidPnpId);
        }
        if self.connector_id == 0 {
            return Err(ValidationError::Zero {
                field: "connector ID",
            });
        }
        validate_text("connector name", &self.connector_name, MAX_BACKEND_ID_BYTES)?;
        if self.product_code == 0 {
            return Err(ValidationError::Zero {
                field: "EDID product code",
            });
        }
        if self.serial == 0 {
            return Err(ValidationError::Zero {
                field: "EDID serial",
            });
        }
        Ok(())
    }
}

/// Current non-sensitive state of one explicitly added cast display.
///
/// The embedded Device record is retained and projected as unavailable when
/// it disappears from discovery, so a configured row remains addressable.
/// Subscribe to `StateChanged` before calling `GetState`, discard queued
/// states at or below the returned `revision`, and then accept only strictly
/// newer states. Revisions may jump when unrelated Devices change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CastDisplayState {
    pub revision: u64,
    pub device: DeviceInfo,
    pub attachment_state: DisplayAttachmentState,
    pub route_state: DisplayRouteState,
    pub routed_mode: Option<RoutedDisplayMode>,
}

impl CastDisplayState {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_nonzero("cast-display state revision", self.revision)?;
        self.device.validate()?;
        if self.device.device_revision > self.revision {
            return Err(ValidationError::FutureDisplayDeviceRevision {
                device_revision: self.device.device_revision,
                state_revision: self.revision,
            });
        }
        let route_shape_is_valid = matches!(
            (self.route_state, self.routed_mode),
            (DisplayRouteState::Disabled, None) | (DisplayRouteState::Active, Some(_))
        );
        if !route_shape_is_valid {
            return Err(ValidationError::InvalidDisplayRoute);
        }
        if self.attachment_state != DisplayAttachmentState::Attached
            && self.route_state != DisplayRouteState::Disabled
        {
            return Err(ValidationError::InvalidDisplayRoute);
        }
        if let Some(mode) = self.routed_mode {
            if mode.width == 0
                || mode.height == 0
                || mode.width > 8192
                || mode.height > 8192
                || mode.refresh_millihz == 0
                || mode.refresh_millihz > 1_000_000
            {
                return Err(ValidationError::InvalidDisplayMode);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CastDisplaySnapshot {
    pub displays: Vec<CastDisplayInfo>,
}

impl CastDisplaySnapshot {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.displays.len() > MAX_PUBLIC_DISPLAYS {
            return Err(ValidationError::TooMany {
                field: "cast displays",
                actual: self.displays.len(),
                limit: MAX_PUBLIC_DISPLAYS,
            });
        }
        let mut identities = HashSet::with_capacity(self.displays.len());
        let mut previous = None;
        for display in &self.displays {
            display.validate()?;
            if !identities.insert(display.display_id.as_str()) {
                return Err(ValidationError::DuplicateDisplay(
                    display.display_id.clone(),
                ));
            }
            if previous.is_some_and(|value| value >= display.display_id.as_str()) {
                return Err(ValidationError::UnorderedDisplays);
            }
            previous = Some(display.display_id.as_str());
        }
        Ok(())
    }
}

#[zbus::proxy(
    interface = "io.github.pronkproject.Pronk1.Manager",
    default_service = "io.github.pronkproject.Pronk1",
    default_path = "/io/github/pronkproject/Pronk1",
    gen_blocking = false
)]
pub trait Manager1 {
    #[zbus(name = "GetVersion")]
    fn get_version(&self) -> zbus::Result<ApiVersion>;

    #[zbus(name = "ListDevices")]
    fn list_devices(&self) -> zbus::Result<DeviceSnapshot>;

    #[zbus(name = "ListDisplays")]
    fn list_displays(&self) -> zbus::Result<CastDisplaySnapshot>;

    #[zbus(name = "AddDisplay")]
    fn add_display(
        &self,
        device: DeviceSelection,
        options: DisplaySetupOptions,
    ) -> zbus::Result<OwnedObjectPath>;

    #[zbus(name = "RemoveDisplay")]
    fn remove_display(&self, display_id: String) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceAdded")]
    fn device_added(&self, inventory_revision: u64, device: DeviceInfo) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceChanged")]
    fn device_changed(&self, inventory_revision: u64, device: DeviceInfo) -> zbus::Result<()>;

    #[zbus(signal, name = "DeviceRemoved")]
    fn device_removed(
        &self,
        inventory_revision: u64,
        backend_id: String,
        device_id: String,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayAdded")]
    fn display_added(&self, display: CastDisplayInfo) -> zbus::Result<()>;

    #[zbus(signal, name = "DisplayRemoved")]
    fn display_removed(&self, display_id: String) -> zbus::Result<()>;
}

mod operation_proxy {
    use super::*;

    #[zbus::proxy(
        interface = "io.github.pronkproject.Pronk1.Operation",
        default_service = "io.github.pronkproject.Pronk1",
        gen_blocking = false
    )]
    pub trait Operation1 {
        #[zbus(name = "GetState")]
        fn get_state(&self) -> zbus::Result<OperationState>;

        #[zbus(name = "Cancel")]
        fn cancel(&self) -> zbus::Result<bool>;

        #[zbus(signal, name = "StateChanged")]
        fn state_changed(&self, state: OperationState) -> zbus::Result<()>;
    }
}

mod cast_display_proxy {
    use super::*;

    #[zbus::proxy(
        interface = "io.github.pronkproject.Pronk1.CastDisplay",
        default_service = "io.github.pronkproject.Pronk1",
        gen_blocking = false
    )]
    pub trait CastDisplay1 {
        #[zbus(name = "GetInfo")]
        fn get_info(&self) -> zbus::Result<CastDisplayInfo>;

        #[zbus(name = "GetState")]
        fn get_state(&self) -> zbus::Result<CastDisplayState>;

        #[zbus(name = "Remove")]
        fn remove(&self) -> zbus::Result<()>;

        #[zbus(signal, name = "Removed")]
        fn removed(&self) -> zbus::Result<()>;

        #[zbus(signal, name = "StateChanged")]
        fn state_changed(&self, state: CastDisplayState) -> zbus::Result<()>;
    }
}

mod media_session_proxy {
    use super::*;

    #[zbus::proxy(
        interface = "io.github.pronkproject.Pronk1.MediaSession",
        default_service = "io.github.pronkproject.Pronk1",
        gen_blocking = false
    )]
    pub trait MediaSession1 {
        #[zbus(name = "GetState")]
        fn get_state(&self) -> zbus::Result<MediaSessionState>;

        #[zbus(signal, name = "StateChanged")]
        fn state_changed(&self, state: MediaSessionState) -> zbus::Result<()>;
    }
}

pub use cast_display_proxy::CastDisplay1Proxy;
pub use media_session_proxy::MediaSession1Proxy;
pub use operation_proxy::Operation1Proxy;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
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
    #[error("backend ID contains a character outside [a-z0-9-]")]
    InvalidBackendId,
    #[error("{field} must be nonzero")]
    Zero { field: &'static str },
    #[error("{field} has {actual} entries; limit is {limit}")]
    TooMany {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("duplicate discovery metadata key {0:?}")]
    DuplicateMetadataKey(String),
    #[error("duplicate device {backend_id:?}/{device_id:?}")]
    DuplicateDevice {
        backend_id: String,
        device_id: String,
    },
    #[error(
        "device {backend_id:?}/{device_id:?} revision {device_revision} is newer than inventory revision {inventory_revision}"
    )]
    FutureDeviceRevision {
        backend_id: String,
        device_id: String,
        device_revision: u64,
        inventory_revision: u64,
    },
    #[error("devices are not ordered by backend ID and device ID")]
    UnorderedDevices,
    #[error("cast-display ID is not a canonical random UUID")]
    InvalidDisplayId,
    #[error("PNP ID must contain exactly three uppercase ASCII letters")]
    InvalidPnpId,
    #[error("duplicate cast display {0:?}")]
    DuplicateDisplay(String),
    #[error("cast displays are not ordered by display ID")]
    UnorderedDisplays,
    #[error("{field} presence does not match its identity source")]
    InvalidIdentitySource { field: &'static str },
    #[error("operation error code does not match its stage")]
    InvalidOperationError,
    #[error(
        "cast-display Device revision {device_revision} is newer than state revision {state_revision}"
    )]
    FutureDisplayDeviceRevision {
        device_revision: u64,
        state_revision: u64,
    },
    #[error("cast-display route state and routed mode are inconsistent")]
    InvalidDisplayRoute,
    #[error("cast-display routed mode is invalid")]
    InvalidDisplayMode,
    #[error("starting, running, or suspended media-session phase has generation zero")]
    InvalidMediaGeneration,
    #[error("media-session error presence does not match its phase")]
    InvalidMediaError,
}

fn validate_backend_id(value: &str) -> Result<(), ValidationError> {
    validate_text("backend ID", value, MAX_BACKEND_ID_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ValidationError::InvalidBackendId);
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str, limit: usize) -> Result<(), ValidationError> {
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

fn validate_optional_text(
    field: &'static str,
    value: &str,
    limit: usize,
) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Ok(());
    }
    validate_text(field, value, limit)
}

fn validate_display_id(value: &str) -> Result<(), ValidationError> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || [8, 13, 18, 23]
            .into_iter()
            .any(|index| bytes[index] != b'-')
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 8 | 13 | 18 | 23) && !byte.is_ascii_hexdigit())
        || bytes[14] != b'4'
        || !matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
    {
        return Err(ValidationError::InvalidDisplayId);
    }
    Ok(())
}

fn validate_identity_source(
    field: &'static str,
    value: &str,
    source: DisplayIdentitySource,
) -> Result<(), ValidationError> {
    let valid = if value.is_empty() {
        source == DisplayIdentitySource::Absent
    } else {
        matches!(
            source,
            DisplayIdentitySource::SetupEndpoint | DisplayIdentitySource::AuthenticatedDeviceInfo
        )
    };
    if !valid {
        return Err(ValidationError::InvalidIdentitySource { field });
    }
    Ok(())
}

fn validate_nonzero(field: &'static str, value: u64) -> Result<(), ValidationError> {
    if value == 0 {
        return Err(ValidationError::Zero { field });
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
            connection_generation: 1,
            discovery_generation: 2,
            device_revision: 3,
            metadata: Vec::new(),
        }
    }

    #[test]
    fn validates_bounded_unique_public_inventory() {
        DeviceSnapshot {
            inventory_revision: 3,
            devices: vec![device("one"), device("two")],
        }
        .validate()
        .unwrap();

        let duplicate = DeviceSnapshot {
            inventory_revision: 3,
            devices: vec![device("one"), device("one")],
        };
        assert!(matches!(
            duplicate.validate(),
            Err(ValidationError::DuplicateDevice { .. })
        ));

        let future = DeviceSnapshot {
            inventory_revision: 2,
            devices: vec![device("one")],
        };
        assert!(matches!(
            future.validate(),
            Err(ValidationError::FutureDeviceRevision { .. })
        ));

        let unordered = DeviceSnapshot {
            inventory_revision: 3,
            devices: vec![device("two"), device("one")],
        };
        assert_eq!(unordered.validate(), Err(ValidationError::UnorderedDevices));
    }

    #[test]
    fn public_wire_signatures_are_stable() {
        assert_eq!(ApiVersion::CURRENT.minor, 4);
        assert_eq!(
            ApiVersion::CURRENT.features & API_FEATURE_CAST_DISPLAY_STATE,
            API_FEATURE_CAST_DISPLAY_STATE
        );
        assert_eq!(
            ApiVersion::CURRENT.features & API_FEATURE_MEDIA_SESSION_STATE,
            API_FEATURE_MEDIA_SESSION_STATE
        );
        assert_eq!(ApiVersion::SIGNATURE, "(qqt)");
        assert_eq!(DeviceAvailability::SIGNATURE, "u");
        assert_eq!(DiscoveryMetadataEntry::SIGNATURE, "(ss)");
        assert_eq!(DeviceInfo::SIGNATURE, "(sssuttta(ss))");
        assert_eq!(DeviceSelection::SIGNATURE, "(ssttt)");
        assert_eq!(DeviceSnapshot::SIGNATURE, "(ta(sssuttta(ss)))");
        assert_eq!(DisplaySetupOptions::SIGNATURE, "(b)");
        assert_eq!(OperationStage::SIGNATURE, "u");
        assert_eq!(OperationErrorCode::SIGNATURE, "u");
        assert_eq!(DisplayIdentitySource::SIGNATURE, "u");
        assert_eq!(DisplayIdentitySource::SetupEndpoint as u32, 1);
        assert_eq!(PnpResolutionSource::SIGNATURE, "u");
        assert_eq!(DisplayAttachmentState::SIGNATURE, "u");
        assert_eq!(DisplayRouteState::SIGNATURE, "u");
        assert_eq!(RoutedDisplayMode::SIGNATURE, "(uuuu)");
        assert_eq!(MediaSessionPhase::SIGNATURE, "u");
        assert_eq!(MediaSessionState::SIGNATURE, "(tutbs)");
        assert_eq!(OperationState::SIGNATURE, "(suus)");
        assert_eq!(CastDisplayInfo::SIGNATURE, "(sssssususuusuquu)");
        assert_eq!(CastDisplayState::SIGNATURE, "(t(sssuttta(ss))uua(uuuu))");
        assert_eq!(CastDisplaySnapshot::SIGNATURE, "(a(sssssususuusuquu))");
    }

    #[test]
    fn media_session_state_is_bounded_and_phase_consistent() {
        MediaSessionState {
            revision: 1,
            phase: MediaSessionPhase::Inactive,
            media_generation: 0,
            audio_enabled: true,
            error: String::new(),
        }
        .validate()
        .unwrap();
        MediaSessionState {
            revision: 9,
            phase: MediaSessionPhase::Running,
            media_generation: 3,
            audio_enabled: true,
            error: String::new(),
        }
        .validate()
        .unwrap();
        MediaSessionState {
            revision: 10,
            phase: MediaSessionPhase::Failed,
            media_generation: 3,
            audio_enabled: false,
            error: "receiver stopped acknowledging media".into(),
        }
        .validate()
        .unwrap();
        MediaSessionState {
            revision: 11,
            phase: MediaSessionPhase::Recovering,
            media_generation: 0,
            audio_enabled: false,
            error: String::new(),
        }
        .validate()
        .unwrap();

        let zero_generation = MediaSessionState {
            revision: 1,
            phase: MediaSessionPhase::Running,
            media_generation: 0,
            audio_enabled: false,
            error: String::new(),
        };
        assert_eq!(
            zero_generation.validate(),
            Err(ValidationError::InvalidMediaGeneration)
        );

        let leaked_error = MediaSessionState {
            revision: 1,
            phase: MediaSessionPhase::Recovering,
            media_generation: 2,
            audio_enabled: false,
            error: "stale diagnostic".into(),
        };
        assert_eq!(
            leaked_error.validate(),
            Err(ValidationError::InvalidMediaError)
        );
    }

    #[test]
    fn cast_display_state_retains_one_revisioned_device() {
        let mut current = device("living-room");
        current.availability = DeviceAvailability::Unavailable;
        CastDisplayState {
            revision: current.device_revision,
            device: current.clone(),
            attachment_state: DisplayAttachmentState::Attached,
            route_state: DisplayRouteState::Disabled,
            routed_mode: None,
        }
        .validate()
        .unwrap();

        CastDisplayState {
            revision: current.device_revision,
            device: current.clone(),
            attachment_state: DisplayAttachmentState::Attached,
            route_state: DisplayRouteState::Active,
            routed_mode: Some(RoutedDisplayMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            }),
        }
        .validate()
        .unwrap();

        let detached_route = CastDisplayState {
            revision: current.device_revision,
            device: current.clone(),
            attachment_state: DisplayAttachmentState::Detached,
            route_state: DisplayRouteState::Active,
            routed_mode: Some(RoutedDisplayMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            }),
        };
        assert_eq!(
            detached_route.validate(),
            Err(ValidationError::InvalidDisplayRoute)
        );

        let stale_state = CastDisplayState {
            revision: current.device_revision - 1,
            device: current,
            attachment_state: DisplayAttachmentState::Attached,
            route_state: DisplayRouteState::Disabled,
            routed_mode: None,
        };
        assert!(matches!(
            stale_state.validate(),
            Err(ValidationError::FutureDisplayDeviceRevision { .. })
        ));

        assert_eq!(
            cast_display_object_path("01234567-89ab-4def-8123-456789abcdef")
                .unwrap()
                .as_str(),
            "/io/github/pronkproject/Pronk1/display/0123456789ab4def8123456789abcdef"
        );
    }

    #[test]
    fn operation_state_requires_consistent_typed_terminal_errors() {
        let display_id = "01234567-89ab-4def-8123-456789abcdef".to_owned();
        OperationState {
            display_id: display_id.clone(),
            stage: OperationStage::Validating,
            error_code: OperationErrorCode::None,
            error: String::new(),
        }
        .validate()
        .unwrap();
        OperationState {
            display_id: display_id.clone(),
            stage: OperationStage::Cancelled,
            error_code: OperationErrorCode::CallerExited,
            error: "setup caller exited".into(),
        }
        .validate()
        .unwrap();
        OperationState {
            display_id: display_id.clone(),
            stage: OperationStage::Failed,
            error_code: OperationErrorCode::DeviceChanged,
            error: "selected Device changed".into(),
        }
        .validate()
        .unwrap();

        for invalid in [
            OperationState {
                display_id: display_id.clone(),
                stage: OperationStage::Added,
                error_code: OperationErrorCode::None,
                error: "unexpected diagnostic".into(),
            },
            OperationState {
                display_id: display_id.clone(),
                stage: OperationStage::Cancelled,
                error_code: OperationErrorCode::Cancelled,
                error: String::new(),
            },
            OperationState {
                display_id,
                stage: OperationStage::Failed,
                error_code: OperationErrorCode::Cancelled,
                error: "wrong terminal code".into(),
            },
        ] {
            assert_eq!(
                invalid.validate(),
                Err(ValidationError::InvalidOperationError)
            );
        }
    }

    #[test]
    fn selection_copies_only_the_selected_device_token() {
        let device = device("living-room");
        let selection = DeviceSelection::from_device(&device);
        selection.validate().unwrap();
        assert_eq!(selection.backend_id, "mock");
        assert_eq!(selection.device_id, "living-room");
        assert_eq!(selection.connection_generation, 1);
        assert_eq!(selection.discovery_generation, 2);
        assert_eq!(selection.device_revision, 3);
    }
}
