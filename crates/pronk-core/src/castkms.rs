//! Safe connector-scoped operations over an inherited CastKMS grant holder.

use std::num::NonZeroU32;
use std::os::fd::AsRawFd;

use castkms_sys::{
    drm_ioctl_castkms_capture_attach_monitor, drm_ioctl_castkms_capture_detach_monitor,
    drm_ioctl_castkms_capture_set_output_edid, drm_ioctl_castkms_get_grant,
    drm_ioctl_mode_getconnector, drm_ioctl_mode_getcrtc, drm_ioctl_mode_getencoder,
    DrmCastkmsCaptureAttachMonitor, DrmCastkmsCaptureDetachMonitor, DrmCastkmsCaptureSetOutputEdid,
    DrmCastkmsGetGrant, DrmModeCrtc, DrmModeGetConnector, DrmModeGetEncoder,
    CAPTURE_MAX_DISPLAY_NAME_SIZE, DRM_MODE_CONNECTED, DRM_MODE_DISCONNECTED,
    DRM_MODE_UNKNOWN_CONNECTION, GRANT_FLAGS_MASK, GRANT_MANAGE_ATTACHMENT, GRANT_STATE_ACTIVE,
    GRANT_STATE_PENDING, GRANT_STATE_REVOKED, GRANT_STATE_SUSPENDED_FOREIGN_CONTENT,
    GRANT_STATE_SUSPENDED_NO_MASTER, GRANT_STATE_SUSPENDED_OTHER_MASTER, GRANT_UPDATE_EDID,
};
use nix::errno::Errno;
use thiserror::Error;

use crate::grant::GrantLease;

#[path = "castkms_event.rs"]
mod event;

#[path = "castkms_capture.rs"]
mod capture;

#[path = "castkms_cec.rs"]
mod cec;

pub use capture::{
    CaptureBufferExport, CaptureBufferInfo, CaptureBufferLayout, CaptureBufferState,
    CaptureCapabilities, CaptureCompletion, CaptureError, CaptureFormat, CaptureProtocolError,
    CaptureQueue, CaptureReady, CaptureRelease, CaptureStopOutcome, CaptureStreamInfo,
    CaptureSynchronization, CaptureSyncobjTimelines, CursorCaptureMode, ExplicitCaptureFence,
    GrantCaptureReconciliation, GrantStateEvidence, GrantStateReconciliation, ImplicitCaptureFence,
    RetiredCaptureStream, MAX_CAPTURE_BUFFER_BYTES, MAX_CAPTURE_FORMATS,
    MAX_OUTSTANDING_CAPTURE_REQUESTS, MAX_TRACKED_CAPTURE_BUFFERS,
};
pub use cec::{
    CecCapabilities, CecCompletion, CecTransmitAdmission, CecTransportBinding,
    CEC_REQUIRED_CAPABILITIES,
};

pub use event::{
    AsyncCastKmsClient, CaptureFrameEvent, CastKmsEvent, CecTransmitEvent, EventDecodeError,
    EventDecoder, EventReadError, GrantRevokedEvent, GrantStateEvent, UnknownEvent,
    DRM_EVENT_HEADER_SIZE, DRM_EVENT_READ_SIZE, MAX_DRM_EVENT_SIZE, MAX_EVENTS_PER_READINESS,
};

pub const EDID_BLOCK_SIZE: usize = 128;
pub const EDID_MAX_BLOCKS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantState {
    Pending,
    Active,
    SuspendedNoMaster,
    SuspendedOtherMaster,
    SuspendedForeignContent,
    Revoked,
}

impl GrantState {
    pub fn is_terminal(self) -> bool {
        self == Self::Revoked
    }
}

impl TryFrom<u32> for GrantState {
    type Error = CastKmsError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            GRANT_STATE_PENDING => Ok(Self::Pending),
            GRANT_STATE_ACTIVE => Ok(Self::Active),
            GRANT_STATE_SUSPENDED_NO_MASTER => Ok(Self::SuspendedNoMaster),
            GRANT_STATE_SUSPENDED_OTHER_MASTER => Ok(Self::SuspendedOtherMaster),
            GRANT_STATE_SUSPENDED_FOREIGN_CONTENT => Ok(Self::SuspendedForeignContent),
            GRANT_STATE_REVOKED => Ok(Self::Revoked),
            _ => Err(CastKmsError::UnknownGrantState(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantInfo {
    pub grant_id: u32,
    pub connector_id: u32,
    pub output_index: u32,
    pub rights: u32,
    pub state: GrantState,
    pub flags: u32,
}

/// Whether the grant-scoped connector currently publishes a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorAttachmentState {
    Detached,
    Attached,
    Unknown,
}

/// One active route observed through the grant holder.
///
/// The CRTC identifier remains an internal capture target. Public adapters
/// should project only the mode and whether a route is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveOutputRoute {
    pub crtc_id: NonZeroU32,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub refresh_millihz: NonZeroU32,
    pub mode_flags: u32,
}

/// Authoritative attachment and active-route snapshot for this grant's exact
/// connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTopology {
    pub attachment: MonitorAttachmentState,
    pub route: Option<ActiveOutputRoute>,
}

/// A complete EDID accepted for connector publication.
///
/// Pronk performs the cheap framing, extension-count, and checksum checks here.
/// CastKMS remains the final semantic validator when the blob is submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEdid {
    bytes: Box<[u8]>,
}

impl ValidatedEdid {
    pub fn new(bytes: Vec<u8>) -> Result<Self, EdidError> {
        validate_edid(&bytes)?;
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl TryFrom<Vec<u8>> for ValidatedEdid {
    type Error = EdidError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::new(bytes)
    }
}

impl TryFrom<&[u8]> for ValidatedEdid {
    type Error = EdidError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::new(bytes.to_vec())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EdidError {
    #[error("EDID size {0} is not a 128-byte multiple in 128..=512")]
    InvalidSize(usize),
    #[error("EDID base-block header is invalid")]
    InvalidHeader,
    #[error("EDID declares {declared} extension blocks but contains {actual}")]
    ExtensionCount { declared: usize, actual: usize },
    #[error("EDID block {block} has an invalid checksum")]
    InvalidChecksum { block: usize },
}

#[derive(Debug, Error)]
pub enum CastKmsError {
    #[error("query CastKMS grant: {0}")]
    QueryGrant(Errno),
    #[error("CastKMS grant query returned invalid {0}")]
    InvalidGrant(&'static str),
    #[error("CastKMS returned unknown grant state {0}")]
    UnknownGrantState(u32),
    #[error("CastKMS grant is already revoked")]
    RevokedGrant,
    #[error("CastKMS grant lacks rights 0x{required:08x}; actual rights are 0x{actual:08x}")]
    MissingRights { required: u32, actual: u32 },
    #[error("invalid assigned CastKMS display name: {0}")]
    InvalidDisplayName(&'static str),
    #[error("attach CastKMS monitor: {0}")]
    AttachMonitor(Errno),
    #[error("set CastKMS output EDID: {0}")]
    SetOutputEdid(Errno),
    #[error("detach CastKMS monitor: {0}")]
    DetachMonitor(Errno),
    #[error("query CastKMS connector topology: {0}")]
    QueryConnector(Errno),
    #[error("query CastKMS encoder topology: {0}")]
    QueryEncoder(Errno),
    #[error("query CastKMS CRTC topology: {0}")]
    QueryCrtc(Errno),
    #[error("CastKMS returned invalid output topology: {0}")]
    InvalidOutputTopology(&'static str),
    #[error("query CastKMS CEC capabilities: {0}")]
    QueryCecCapabilities(Errno),
    #[error("bind CastKMS CEC transport: {0}")]
    BindCecTransport(Errno),
    #[error("set CastKMS CEC transport state: {0}")]
    SetCecTransportState(Errno),
    #[error("complete CastKMS CEC transmit: {0}")]
    CompleteCecTransmit(Errno),
    #[error("inject CastKMS CEC receive message: {0}")]
    ReceiveCecMessage(Errno),
    #[error("unbind CastKMS CEC transport: {0}")]
    UnbindCecTransport(Errno),
    #[error("CastKMS returned invalid CEC metadata: {0}")]
    InvalidCecMetadata(&'static str),
    #[error("invalid CastKMS CEC lifecycle: {0}")]
    InvalidCecState(&'static str),
}

/// The connector-scoped CastKMS client used by the core actor.
///
/// It consumes the lease so all sensitive operations use its inherited holder;
/// there is intentionally no device path or reopen operation in this API.
#[derive(Debug)]
pub struct CastKmsClient {
    lease: GrantLease,
    capture: capture::CaptureTracker,
    cec: cec::CecTracker,
}

impl AsRawFd for CastKmsClient {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.lease.holder().as_raw_fd()
    }
}

impl CastKmsClient {
    pub fn new(lease: GrantLease) -> Result<Self, CastKmsError> {
        let client = Self {
            lease,
            capture: capture::CaptureTracker::default(),
            cec: cec::CecTracker::default(),
        };
        let info = client.query_grant()?;
        if info.state.is_terminal() {
            return Err(CastKmsError::RevokedGrant);
        }
        Ok(client)
    }

    pub fn grant_id(&self) -> u32 {
        self.lease.grant_id()
    }

    pub fn connector_id(&self) -> u32 {
        self.lease.connector_id()
    }

    pub fn output_index(&self) -> u32 {
        self.lease.output_index()
    }

    /// Register this same inherited holder for Tokio event readiness.
    ///
    /// This must be called from within a Tokio runtime with I/O enabled.
    pub fn into_async(self) -> std::io::Result<AsyncCastKmsClient> {
        AsyncCastKmsClient::new(self)
    }

    pub fn query_grant(&self) -> Result<GrantInfo, CastKmsError> {
        let mut query = DrmCastkmsGetGrant::default();
        // SAFETY: `query` has the checked-in UAPI layout and remains writable
        // for the duration of the ioctl. The lease owns the borrowed fd.
        unsafe { drm_ioctl_castkms_get_grant(self.lease.holder().as_raw_fd(), &mut query) }
            .map_err(CastKmsError::QueryGrant)?;

        if query.reserved != 0 {
            return Err(CastKmsError::InvalidGrant("reserved field"));
        }
        if query.flags & !GRANT_FLAGS_MASK != 0 {
            return Err(CastKmsError::InvalidGrant("flags"));
        }
        if query.grant_id != self.lease.grant_id() {
            return Err(CastKmsError::InvalidGrant("grant ID"));
        }
        if query.connector_id != self.lease.connector_id() {
            return Err(CastKmsError::InvalidGrant("connector ID"));
        }
        if query.output_index != self.lease.output_index() {
            return Err(CastKmsError::InvalidGrant("output index"));
        }
        if query.rights != self.lease.rights() {
            return Err(CastKmsError::InvalidGrant("rights"));
        }
        if query.flags != self.lease.flags() {
            return Err(CastKmsError::InvalidGrant("mode flags"));
        }

        Ok(GrantInfo {
            grant_id: query.grant_id,
            connector_id: query.connector_id,
            output_index: query.output_index,
            rights: query.rights,
            state: GrantState::try_from(query.state)?,
            flags: query.flags,
        })
    }

    /// Query attachment and the currently active route without reopening the
    /// DRM device or taking master.
    pub fn query_output_topology(&self) -> Result<OutputTopology, CastKmsError> {
        let mut connector = DrmModeGetConnector {
            connector_id: self.connector_id(),
            ..DrmModeGetConnector::default()
        };
        // SAFETY: the connector structure is writable for the synchronous
        // read-only ioctl and contains no caller pointers.
        unsafe { drm_ioctl_mode_getconnector(self.as_raw_fd(), &mut connector) }
            .map_err(CastKmsError::QueryConnector)?;
        if connector.connector_id != self.connector_id() || connector.pad != 0 {
            return Err(CastKmsError::InvalidOutputTopology(
                "connector identity or padding",
            ));
        }

        let attachment = match connector.connection {
            DRM_MODE_CONNECTED => MonitorAttachmentState::Attached,
            DRM_MODE_DISCONNECTED => MonitorAttachmentState::Detached,
            DRM_MODE_UNKNOWN_CONNECTION => MonitorAttachmentState::Unknown,
            _ => {
                return Err(CastKmsError::InvalidOutputTopology(
                    "connector connection state",
                ))
            }
        };
        if attachment != MonitorAttachmentState::Attached || connector.encoder_id == 0 {
            return Ok(OutputTopology {
                attachment,
                route: None,
            });
        }

        let mut encoder = DrmModeGetEncoder {
            encoder_id: connector.encoder_id,
            ..DrmModeGetEncoder::default()
        };
        // SAFETY: the encoder structure is writable for the synchronous
        // read-only ioctl and contains no pointers.
        unsafe { drm_ioctl_mode_getencoder(self.as_raw_fd(), &mut encoder) }
            .map_err(CastKmsError::QueryEncoder)?;
        if encoder.encoder_id != connector.encoder_id {
            return Err(CastKmsError::InvalidOutputTopology("encoder identity"));
        }
        let Some(crtc_id) = NonZeroU32::new(encoder.crtc_id) else {
            return Ok(OutputTopology {
                attachment,
                route: None,
            });
        };

        let mut crtc = DrmModeCrtc {
            crtc_id: crtc_id.get(),
            ..DrmModeCrtc::default()
        };
        // SAFETY: the CRTC structure is writable for the synchronous
        // read-only ioctl and contains no caller pointers.
        unsafe { drm_ioctl_mode_getcrtc(self.as_raw_fd(), &mut crtc) }
            .map_err(CastKmsError::QueryCrtc)?;
        output_topology_from_kernel(attachment, crtc_id, crtc)
    }

    /// Attach the grant's connector and atomically publish its initial EDID and
    /// user-assigned name.
    pub fn attach_monitor(
        &self,
        edid: &ValidatedEdid,
        display_name: &str,
    ) -> Result<(), CastKmsError> {
        self.require_rights(GRANT_MANAGE_ATTACHMENT | GRANT_UPDATE_EDID)?;
        let display_name = display_name_for_uapi(display_name)?;
        let args = DrmCastkmsCaptureAttachMonitor {
            connector_id: self.lease.connector_id(),
            edid_size: edid.len() as u32,
            display_name_size: display_name.len() as u32,
            edid_ptr: edid_pointer(edid),
            display_name_ptr: bytes_pointer(display_name.as_bytes()),
            ..DrmCastkmsCaptureAttachMonitor::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout. `edid` remains alive
        // and immutable, as does `display_name`, until the synchronous ioctl
        // has copied both buffers.
        unsafe { drm_ioctl_castkms_capture_attach_monitor(self.lease.holder().as_raw_fd(), &args) }
            .map_err(CastKmsError::AttachMonitor)?;
        Ok(())
    }

    /// Replace the complete EDID while retaining the existing attachment.
    pub fn set_output_edid(&self, edid: &ValidatedEdid) -> Result<(), CastKmsError> {
        self.require_rights(GRANT_UPDATE_EDID)?;
        let args = DrmCastkmsCaptureSetOutputEdid {
            connector_id: self.lease.connector_id(),
            edid_size: edid.len() as u32,
            edid_ptr: edid_pointer(edid),
            ..DrmCastkmsCaptureSetOutputEdid::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout. `edid` remains alive
        // and immutable until the synchronous ioctl has copied it.
        unsafe {
            drm_ioctl_castkms_capture_set_output_edid(self.lease.holder().as_raw_fd(), &args)
        }
        .map_err(CastKmsError::SetOutputEdid)?;
        Ok(())
    }

    /// Clear the EDID without unplugging the grant-owned monitor.
    pub fn clear_output_edid(&self) -> Result<(), CastKmsError> {
        self.require_rights(GRANT_UPDATE_EDID)?;
        let args = DrmCastkmsCaptureSetOutputEdid {
            connector_id: self.lease.connector_id(),
            ..DrmCastkmsCaptureSetOutputEdid::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and contains no user
        // pointer for the zero-length clear operation.
        unsafe {
            drm_ioctl_castkms_capture_set_output_edid(self.lease.holder().as_raw_fd(), &args)
        }
        .map_err(CastKmsError::SetOutputEdid)?;
        Ok(())
    }

    pub fn detach_monitor(&self) -> Result<(), CastKmsError> {
        if self.cec.is_bound() {
            return Err(CastKmsError::InvalidCecState(
                "CEC transport must be unbound before monitor detach",
            ));
        }
        self.require_rights(GRANT_MANAGE_ATTACHMENT)?;
        let args = DrmCastkmsCaptureDetachMonitor {
            connector_id: self.lease.connector_id(),
            ..DrmCastkmsCaptureDetachMonitor::default()
        };
        // SAFETY: `args` has the checked-in UAPI layout and remains valid for
        // the duration of the synchronous ioctl.
        unsafe { drm_ioctl_castkms_capture_detach_monitor(self.lease.holder().as_raw_fd(), &args) }
            .map_err(CastKmsError::DetachMonitor)?;
        Ok(())
    }

    fn require_rights(&self, required: u32) -> Result<(), CastKmsError> {
        let actual = self.lease.rights();
        if actual & required == required {
            Ok(())
        } else {
            Err(CastKmsError::MissingRights { required, actual })
        }
    }
}

fn output_topology_from_kernel(
    attachment: MonitorAttachmentState,
    expected_crtc_id: NonZeroU32,
    crtc: DrmModeCrtc,
) -> Result<OutputTopology, CastKmsError> {
    if crtc.crtc_id != expected_crtc_id.get() {
        return Err(CastKmsError::InvalidOutputTopology("CRTC identity"));
    }
    match crtc.mode_valid {
        0 => Ok(OutputTopology {
            attachment,
            route: None,
        }),
        1 => {
            let width = NonZeroU32::new(u32::from(crtc.mode.hdisplay))
                .ok_or(CastKmsError::InvalidOutputTopology("active mode width"))?;
            let height = NonZeroU32::new(u32::from(crtc.mode.vdisplay))
                .ok_or(CastKmsError::InvalidOutputTopology("active mode height"))?;
            let refresh_hz = if crtc.mode.vrefresh == 0 {
                60
            } else {
                crtc.mode.vrefresh
            };
            let refresh_millihz = NonZeroU32::new(
                refresh_hz
                    .checked_mul(1_000)
                    .ok_or(CastKmsError::InvalidOutputTopology("active mode refresh"))?,
            )
            .expect("a nonzero refresh multiplied by 1000 remains nonzero");
            Ok(OutputTopology {
                attachment,
                route: Some(ActiveOutputRoute {
                    crtc_id: expected_crtc_id,
                    width,
                    height,
                    refresh_millihz,
                    mode_flags: crtc.mode.flags,
                }),
            })
        }
        _ => Err(CastKmsError::InvalidOutputTopology("CRTC mode-valid field")),
    }
}

fn validate_edid(bytes: &[u8]) -> Result<(), EdidError> {
    if bytes.is_empty()
        || bytes.len() > EDID_BLOCK_SIZE * EDID_MAX_BLOCKS
        || bytes.len() % EDID_BLOCK_SIZE != 0
    {
        return Err(EdidError::InvalidSize(bytes.len()));
    }

    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    if bytes[..HEADER.len()] != HEADER {
        return Err(EdidError::InvalidHeader);
    }

    let actual_extensions = bytes.len() / EDID_BLOCK_SIZE - 1;
    let declared_extensions = usize::from(bytes[126]);
    if declared_extensions != actual_extensions {
        return Err(EdidError::ExtensionCount {
            declared: declared_extensions,
            actual: actual_extensions,
        });
    }

    for (block, bytes) in bytes.chunks_exact(EDID_BLOCK_SIZE).enumerate() {
        let checksum = bytes.iter().copied().fold(0_u8, u8::wrapping_add);
        if checksum != 0 {
            return Err(EdidError::InvalidChecksum { block });
        }
    }

    Ok(())
}

fn edid_pointer(edid: &ValidatedEdid) -> u64 {
    bytes_pointer(edid.as_bytes())
}

fn bytes_pointer(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.as_ptr() as usize)
        .expect("supported Rust targets have pointers no wider than 64 bits")
}

fn display_name_for_uapi(display_name: &str) -> Result<&str, CastKmsError> {
    if display_name.is_empty() {
        return Err(CastKmsError::InvalidDisplayName("name is empty"));
    }
    if display_name.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(CastKmsError::InvalidDisplayName(
            "name contains an ASCII control byte",
        ));
    }

    let mut end = display_name.len().min(CAPTURE_MAX_DISPLAY_NAME_SIZE);
    while !display_name.is_char_boundary(end) {
        end -= 1;
    }
    Ok(&display_name[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed_edid(block_count: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; block_count * EDID_BLOCK_SIZE];
        bytes[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        bytes[126] = (block_count - 1) as u8;
        for block in bytes.chunks_exact_mut(EDID_BLOCK_SIZE) {
            let sum = block[..EDID_BLOCK_SIZE - 1]
                .iter()
                .copied()
                .fold(0_u8, u8::wrapping_add);
            block[EDID_BLOCK_SIZE - 1] = 0_u8.wrapping_sub(sum);
        }
        bytes
    }

    #[test]
    fn accepts_one_to_four_well_framed_blocks() {
        for block_count in 1..=EDID_MAX_BLOCKS {
            let edid = ValidatedEdid::new(framed_edid(block_count)).unwrap();
            assert_eq!(edid.len(), block_count * EDID_BLOCK_SIZE);
        }
    }

    #[test]
    fn rejects_invalid_sizes() {
        for size in [0, 1, 127, 129, 513] {
            assert_eq!(
                ValidatedEdid::new(vec![0; size]).unwrap_err(),
                EdidError::InvalidSize(size)
            );
        }
    }

    #[test]
    fn rejects_an_invalid_header() {
        let mut bytes = framed_edid(1);
        bytes[0] = 1;
        assert_eq!(
            ValidatedEdid::new(bytes).unwrap_err(),
            EdidError::InvalidHeader
        );
    }

    #[test]
    fn rejects_a_mismatched_extension_count() {
        let mut bytes = framed_edid(2);
        bytes[126] = 0;
        bytes[127] = bytes[127].wrapping_add(1);
        assert_eq!(
            ValidatedEdid::new(bytes).unwrap_err(),
            EdidError::ExtensionCount {
                declared: 0,
                actual: 1,
            }
        );
    }

    #[test]
    fn rejects_a_bad_checksum_in_any_block() {
        let mut bytes = framed_edid(2);
        bytes[EDID_BLOCK_SIZE + 3] = 1;
        assert_eq!(
            ValidatedEdid::new(bytes).unwrap_err(),
            EdidError::InvalidChecksum { block: 1 }
        );
    }

    #[test]
    fn bounds_assigned_display_names_at_a_utf8_boundary() {
        assert_eq!(
            display_name_for_uapi("Apartment Living Room TV").unwrap(),
            "Apartment Living Room TV"
        );

        let long_name = format!("{}é", "x".repeat(CAPTURE_MAX_DISPLAY_NAME_SIZE - 1));
        assert_eq!(
            display_name_for_uapi(&long_name).unwrap(),
            "x".repeat(CAPTURE_MAX_DISPLAY_NAME_SIZE - 1)
        );
    }

    #[test]
    fn rejects_empty_or_control_bearing_display_names() {
        assert!(matches!(
            display_name_for_uapi(""),
            Err(CastKmsError::InvalidDisplayName("name is empty"))
        ));
        assert!(matches!(
            display_name_for_uapi("Living Room\nTV"),
            Err(CastKmsError::InvalidDisplayName(
                "name contains an ASCII control byte"
            ))
        ));
    }

    #[test]
    fn maps_every_known_grant_state() {
        let states = [
            GrantState::Pending,
            GrantState::Active,
            GrantState::SuspendedNoMaster,
            GrantState::SuspendedOtherMaster,
            GrantState::SuspendedForeignContent,
            GrantState::Revoked,
        ];
        for (value, expected) in states.into_iter().enumerate() {
            assert_eq!(GrantState::try_from(value as u32).unwrap(), expected);
        }
        assert!(matches!(
            GrantState::try_from(6),
            Err(CastKmsError::UnknownGrantState(6))
        ));
    }

    #[test]
    fn decodes_active_and_disabled_routes_without_exposing_a_reopen() {
        let crtc_id = NonZeroU32::new(17).unwrap();
        let active = DrmModeCrtc {
            crtc_id: crtc_id.get(),
            mode_valid: 1,
            mode: castkms_sys::DrmModeModeInfo {
                hdisplay: 1920,
                vdisplay: 1080,
                vrefresh: 60,
                flags: 5,
                ..castkms_sys::DrmModeModeInfo::default()
            },
            ..DrmModeCrtc::default()
        };
        assert_eq!(
            output_topology_from_kernel(MonitorAttachmentState::Attached, crtc_id, active).unwrap(),
            OutputTopology {
                attachment: MonitorAttachmentState::Attached,
                route: Some(ActiveOutputRoute {
                    crtc_id,
                    width: NonZeroU32::new(1920).unwrap(),
                    height: NonZeroU32::new(1080).unwrap(),
                    refresh_millihz: NonZeroU32::new(60_000).unwrap(),
                    mode_flags: 5,
                }),
            }
        );

        let disabled = DrmModeCrtc {
            crtc_id: crtc_id.get(),
            mode_valid: 0,
            ..DrmModeCrtc::default()
        };
        assert_eq!(
            output_topology_from_kernel(MonitorAttachmentState::Attached, crtc_id, disabled)
                .unwrap(),
            OutputTopology {
                attachment: MonitorAttachmentState::Attached,
                route: None,
            }
        );
    }

    #[test]
    fn rejects_incoherent_active_route_metadata() {
        let crtc_id = NonZeroU32::new(17).unwrap();
        let wrong_id = DrmModeCrtc {
            crtc_id: 18,
            mode_valid: 1,
            ..DrmModeCrtc::default()
        };
        assert!(matches!(
            output_topology_from_kernel(MonitorAttachmentState::Attached, crtc_id, wrong_id),
            Err(CastKmsError::InvalidOutputTopology("CRTC identity"))
        ));

        let invalid_mode = DrmModeCrtc {
            crtc_id: crtc_id.get(),
            mode_valid: 2,
            ..DrmModeCrtc::default()
        };
        assert!(matches!(
            output_topology_from_kernel(MonitorAttachmentState::Attached, crtc_id, invalid_mode),
            Err(CastKmsError::InvalidOutputTopology("CRTC mode-valid field"))
        ));
    }
}
