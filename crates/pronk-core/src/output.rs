//! Bounded, non-authoritative CastKMS output discovery.
//!
//! Discovery uses a short-lived ordinary primary-node file only to identify
//! the driver and enumerate connectors. It never creates a grant, attaches a
//! monitor, or retains DRM authority. The connector-to-slot mapping comes from
//! CastKMS's read-only output query because connector type IDs and resource
//! array positions are not stable UAPI identities.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use castkms_sys::{
    drm_ioctl_castkms_get_output, drm_ioctl_mode_getconnector, drm_ioctl_mode_getresources,
    drm_ioctl_version, DrmCastkmsGetOutput, DrmModeCardRes, DrmModeGetConnector, DrmModeModeInfo,
    DrmVersion, DRM_MODE_CONNECTED, DRM_MODE_CONNECTOR_VIRTUAL, DRM_MODE_DISCONNECTED,
    DRM_MODE_UNKNOWN_CONNECTION,
};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::{major, minor};
use thiserror::Error;

pub const MAX_DRM_PRIMARY_NODES: usize = 64;
pub const MAX_CASTKMS_OUTPUTS: usize = 64;
const DRM_DRIVER_NAME_CAPACITY: usize = 32;
const RESOURCE_RETRIES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CastKmsOutputId {
    /// Canonical parent-device sysfs path, stable across DRM card renumbering.
    pub device_path: PathBuf,
    /// Stable index assigned once when CastKMS creates the output.
    pub output_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputConnection {
    Connected,
    Disconnected,
    Unknown,
}

impl OutputConnection {
    pub fn is_available(self) -> bool {
        self == Self::Disconnected
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastKmsOutput {
    pub id: CastKmsOutputId,
    pub node_path: PathBuf,
    pub device_major: u32,
    pub device_minor: u32,
    pub connector_id: u32,
    pub connector_name: String,
    pub connection: OutputConnection,
}

impl CastKmsOutput {
    pub fn is_available(&self) -> bool {
        self.connection.is_available()
    }
}

/// Enumerate every CastKMS virtual connector visible to this process.
///
/// Returned records are sorted by stable device path and output index. The
/// primary-node descriptors are all closed before this function returns.
pub fn discover_castkms_outputs() -> Result<Vec<CastKmsOutput>, OutputDiscoveryError> {
    discover_castkms_outputs_at(Path::new("/dev/dri"), Path::new("/sys/class/drm"))
}

fn discover_castkms_outputs_at(
    device_directory: &Path,
    drm_sysfs_class: &Path,
) -> Result<Vec<CastKmsOutput>, OutputDiscoveryError> {
    let mut nodes = Vec::new();
    let entries = match fs::read_dir(device_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(OutputDiscoveryError::ReadDeviceDirectory {
                path: device_directory.to_owned(),
                source,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| OutputDiscoveryError::ReadDeviceDirectory {
            path: device_directory.to_owned(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(index) = primary_node_index(&file_name) else {
            continue;
        };
        nodes.push((index, entry.path(), file_name));
        if nodes.len() > MAX_DRM_PRIMARY_NODES {
            return Err(OutputDiscoveryError::TooManyPrimaryNodes(nodes.len()));
        }
    }
    nodes.sort_by_key(|(index, _, _)| *index);

    let mut outputs = Vec::new();
    for (_, node_path, file_name) in nodes {
        match probe_primary_node(&node_path, &file_name, drm_sysfs_class) {
            Ok(Some(mut card_outputs)) => outputs.append(&mut card_outputs),
            Ok(None) => {}
            Err(source) => {
                return Err(OutputDiscoveryError::ProbeNode {
                    path: node_path,
                    source,
                })
            }
        }
        if outputs.len() > MAX_CASTKMS_OUTPUTS {
            return Err(OutputDiscoveryError::TooManyOutputs(outputs.len()));
        }
    }

    finish_output_inventory(outputs)
}

fn primary_node_index(file_name: &std::ffi::OsStr) -> Option<u32> {
    let bytes = file_name.as_bytes();
    let digits = bytes.strip_prefix(b"card")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn probe_primary_node(
    node_path: &Path,
    file_name: &std::ffi::OsStr,
    drm_sysfs_class: &Path,
) -> Result<Option<Vec<CastKmsOutput>>, CardProbeError> {
    let metadata = fs::symlink_metadata(node_path).map_err(CardProbeError::NodeMetadata)?;
    if !metadata.file_type().is_char_device() {
        return Ok(None);
    }

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags((OFlag::O_CLOEXEC | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW).bits())
        .open(node_path)
    {
        Ok(file) => file,
        Err(_source) if !sysfs_declares_castkms(file_name, drm_sysfs_class) => return Ok(None),
        Err(source) => return Err(CardProbeError::Open(source)),
    };
    if !is_castkms(&file)? {
        return Ok(None);
    }

    let device_path = fs::canonicalize(drm_sysfs_class.join(file_name).join("device"))
        .map_err(CardProbeError::DeviceIdentity)?;
    let device_major =
        u32::try_from(major(metadata.rdev())).map_err(|_| CardProbeError::InvalidDeviceNumber)?;
    let device_minor =
        u32::try_from(minor(metadata.rdev())).map_err(|_| CardProbeError::InvalidDeviceNumber)?;
    let connector_ids = connector_ids(&file)?;
    let mut outputs = Vec::with_capacity(connector_ids.len());
    for connector_id in connector_ids {
        let connector = connector_metadata(&file, connector_id)?;
        if connector.connector_type != DRM_MODE_CONNECTOR_VIRTUAL {
            continue;
        }
        let connection = match connector.connection {
            DRM_MODE_CONNECTED => OutputConnection::Connected,
            DRM_MODE_DISCONNECTED => OutputConnection::Disconnected,
            DRM_MODE_UNKNOWN_CONNECTION => OutputConnection::Unknown,
            value => return Err(CardProbeError::UnknownConnection(value)),
        };
        let output_index = query_output_index(&file, connector_id)?;
        outputs.push(CastKmsOutput {
            id: CastKmsOutputId {
                device_path: device_path.clone(),
                output_index,
            },
            node_path: node_path.to_owned(),
            device_major,
            device_minor,
            connector_id,
            connector_name: format!("Virtual-{}", connector.connector_type_id),
            connection,
        });
    }
    Ok(Some(outputs))
}

fn sysfs_declares_castkms(file_name: &std::ffi::OsStr, drm_sysfs_class: &Path) -> bool {
    fs::read_to_string(drm_sysfs_class.join(file_name).join("device/uevent"))
        .is_ok_and(|contents| contents.lines().any(|line| line == "DRIVER=castkms"))
}

fn is_castkms(file: &File) -> Result<bool, CardProbeError> {
    let mut name = [0_u8; DRM_DRIVER_NAME_CAPACITY];
    let mut version = DrmVersion {
        name_len: name.len(),
        name: name.as_mut_ptr().cast(),
        ..DrmVersion::default()
    };
    // SAFETY: `version` has the native DRM layout and its only non-null
    // pointer names the writable `name` array for the synchronous ioctl.
    unsafe { drm_ioctl_version(file.as_raw_fd(), &mut version) }
        .map_err(CardProbeError::Version)?;
    if version.name_len > name.len() {
        return Err(CardProbeError::DriverNameTooLong(version.name_len));
    }
    Ok(&name[..version.name_len] == b"castkms")
}

fn connector_ids(file: &File) -> Result<Vec<u32>, CardProbeError> {
    let mut resources = DrmModeCardRes::default();
    // SAFETY: `resources` is a writable standard DRM UAPI structure with no
    // pointers set for this count query.
    unsafe { drm_ioctl_mode_getresources(file.as_raw_fd(), &mut resources) }
        .map_err(CardProbeError::Resources)?;

    for _ in 0..RESOURCE_RETRIES {
        let capacity = usize::try_from(resources.count_connectors)
            .map_err(|_| CardProbeError::TooManyConnectors(usize::MAX))?;
        if capacity > MAX_CASTKMS_OUTPUTS {
            return Err(CardProbeError::TooManyConnectors(capacity));
        }
        if capacity == 0 {
            return Ok(Vec::new());
        }
        let mut ids = vec![0_u32; capacity];
        resources.connector_id_ptr = ids.as_mut_ptr() as u64;
        resources.count_connectors = capacity as u32;
        // Do not request unrelated framebuffer, CRTC, or encoder arrays.
        resources.fb_id_ptr = 0;
        resources.crtc_id_ptr = 0;
        resources.encoder_id_ptr = 0;
        resources.count_fbs = 0;
        resources.count_crtcs = 0;
        resources.count_encoders = 0;
        // SAFETY: the connector pointer names `capacity` writable u32s and
        // remains alive through the synchronous ioctl.
        unsafe { drm_ioctl_mode_getresources(file.as_raw_fd(), &mut resources) }
            .map_err(CardProbeError::Resources)?;
        let actual = resources.count_connectors as usize;
        if actual <= capacity {
            ids.truncate(actual);
            if ids.contains(&0) {
                return Err(CardProbeError::ZeroConnectorId);
            }
            return Ok(ids);
        }
    }
    Err(CardProbeError::UnstableResources)
}

fn connector_metadata(
    file: &File,
    connector_id: u32,
) -> Result<DrmModeGetConnector, CardProbeError> {
    // count_modes=0 can force-probe when this short-lived file happens to be
    // DRM master. Supply one scratch mode so discovery remains observational.
    let mut scratch_mode = DrmModeModeInfo::default();
    let mut connector = DrmModeGetConnector {
        modes_ptr: (&mut scratch_mode as *mut DrmModeModeInfo) as u64,
        count_modes: 1,
        connector_id,
        ..DrmModeGetConnector::default()
    };
    // SAFETY: `connector` and the single scratch mode remain writable through
    // the synchronous standard DRM ioctl. No other arrays are requested.
    unsafe { drm_ioctl_mode_getconnector(file.as_raw_fd(), &mut connector) }
        .map_err(CardProbeError::Connector)?;
    if connector.connector_id != connector_id {
        return Err(CardProbeError::ConnectorIdentity {
            expected: connector_id,
            actual: connector.connector_id,
        });
    }
    if connector.pad != 0 {
        return Err(CardProbeError::ConnectorPadding);
    }
    Ok(connector)
}

fn query_output_index(file: &File, connector_id: u32) -> Result<u32, CardProbeError> {
    let mut query = DrmCastkmsGetOutput {
        connector_id,
        ..DrmCastkmsGetOutput::default()
    };
    // SAFETY: `query` exactly matches the checked-in CastKMS UAPI and remains
    // writable for the synchronous ioctl.
    unsafe { drm_ioctl_castkms_get_output(file.as_raw_fd(), &mut query) }
        .map_err(CardProbeError::OutputIndex)?;
    if query.connector_id != connector_id || query.flags != 0 || query.reserved != 0 {
        return Err(CardProbeError::InvalidOutputQuery(
            "identity or reserved fields",
        ));
    }
    Ok(query.output_index)
}

fn finish_output_inventory(
    mut outputs: Vec<CastKmsOutput>,
) -> Result<Vec<CastKmsOutput>, OutputDiscoveryError> {
    outputs.sort_by(|left, right| left.id.cmp(&right.id));
    let mut ids = HashSet::with_capacity(outputs.len());
    let mut connectors = HashSet::with_capacity(outputs.len());
    for output in &outputs {
        if !ids.insert(output.id.clone()) {
            return Err(OutputDiscoveryError::DuplicateOutput(output.id.clone()));
        }
        let connector = (output.id.device_path.clone(), output.connector_id);
        if !connectors.insert(connector) {
            return Err(OutputDiscoveryError::DuplicateConnector {
                device_path: output.id.device_path.clone(),
                connector_id: output.connector_id,
            });
        }
    }
    Ok(outputs)
}

#[derive(Debug, Error)]
pub enum OutputDiscoveryError {
    #[error("read DRM primary-node directory {path:?}: {source}")]
    ReadDeviceDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("found {0} DRM primary nodes; limit is {MAX_DRM_PRIMARY_NODES}")]
    TooManyPrimaryNodes(usize),
    #[error("probe DRM primary node {path:?}: {source}")]
    ProbeNode {
        path: PathBuf,
        #[source]
        source: CardProbeError,
    },
    #[error("found {0} CastKMS outputs; limit is {MAX_CASTKMS_OUTPUTS}")]
    TooManyOutputs(usize),
    #[error("CastKMS reported duplicate stable output {0:?}")]
    DuplicateOutput(CastKmsOutputId),
    #[error("CastKMS device {device_path:?} reported connector {connector_id} twice")]
    DuplicateConnector {
        device_path: PathBuf,
        connector_id: u32,
    },
}

#[derive(Debug, Error)]
pub enum CardProbeError {
    #[error("inspect device node: {0}")]
    NodeMetadata(std::io::Error),
    #[error("open device node: {0}")]
    Open(std::io::Error),
    #[error("query DRM driver version: {0}")]
    Version(Errno),
    #[error("DRM driver name is {0} bytes; buffer is {DRM_DRIVER_NAME_CAPACITY}")]
    DriverNameTooLong(usize),
    #[error("resolve canonical sysfs device identity: {0}")]
    DeviceIdentity(std::io::Error),
    #[error("DRM device number does not fit the CastKMS grant request")]
    InvalidDeviceNumber,
    #[error("query DRM resources: {0}")]
    Resources(Errno),
    #[error("CastKMS reported {0} connectors; limit is {MAX_CASTKMS_OUTPUTS}")]
    TooManyConnectors(usize),
    #[error("CastKMS resource inventory did not stabilize")]
    UnstableResources,
    #[error("CastKMS reported connector ID zero")]
    ZeroConnectorId,
    #[error("query DRM connector: {0}")]
    Connector(Errno),
    #[error("connector query returned ID {actual}; expected {expected}")]
    ConnectorIdentity { expected: u32, actual: u32 },
    #[error("connector query returned nonzero padding")]
    ConnectorPadding,
    #[error("connector query returned unknown connection state {0}")]
    UnknownConnection(u32),
    #[error("query stable CastKMS output index: {0}")]
    OutputIndex(Errno),
    #[error("CastKMS output-index query returned invalid {0}")]
    InvalidOutputQuery(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(device: &str, output_index: u32, connector_id: u32) -> CastKmsOutput {
        CastKmsOutput {
            id: CastKmsOutputId {
                device_path: PathBuf::from(device),
                output_index,
            },
            node_path: PathBuf::from("/dev/dri/card9"),
            device_major: 226,
            device_minor: 9,
            connector_id,
            connector_name: format!("Virtual-{}", output_index + 1),
            connection: OutputConnection::Disconnected,
        }
    }

    #[test]
    fn recognizes_only_primary_node_names() {
        assert_eq!(primary_node_index(std::ffi::OsStr::new("card0")), Some(0));
        assert_eq!(primary_node_index(std::ffi::OsStr::new("card17")), Some(17));
        assert_eq!(primary_node_index(std::ffi::OsStr::new("renderD128")), None);
        assert_eq!(primary_node_index(std::ffi::OsStr::new("card")), None);
        assert_eq!(primary_node_index(std::ffi::OsStr::new("card2x")), None);
    }

    #[test]
    fn sorts_by_stable_device_and_output_identity() {
        let outputs = finish_output_inventory(vec![
            output("/sys/devices/z", 0, 10),
            output("/sys/devices/a", 1, 12),
            output("/sys/devices/a", 0, 11),
        ])
        .unwrap();
        assert_eq!(outputs[0].connector_id, 11);
        assert_eq!(outputs[1].connector_id, 12);
        assert_eq!(outputs[2].connector_id, 10);
        assert!(outputs.iter().all(CastKmsOutput::is_available));
    }

    #[test]
    fn rejects_duplicate_stable_outputs_and_connectors() {
        let duplicate_output = vec![
            output("/sys/devices/castkms", 0, 10),
            output("/sys/devices/castkms", 0, 11),
        ];
        assert!(matches!(
            finish_output_inventory(duplicate_output),
            Err(OutputDiscoveryError::DuplicateOutput(_))
        ));

        let duplicate_connector = vec![
            output("/sys/devices/castkms", 0, 10),
            output("/sys/devices/castkms", 1, 10),
        ];
        assert!(matches!(
            finish_output_inventory(duplicate_connector),
            Err(OutputDiscoveryError::DuplicateConnector { .. })
        ));
    }

    #[test]
    fn only_disconnected_outputs_are_available() {
        assert!(OutputConnection::Disconnected.is_available());
        assert!(!OutputConnection::Connected.is_available());
        assert!(!OutputConnection::Unknown.is_available());
    }

    #[test]
    fn missing_dri_directory_is_an_empty_inventory() {
        let root = std::env::temp_dir().join(format!(
            "pronk-output-missing-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        assert_eq!(
            discover_castkms_outputs_at(&root, Path::new("/sys/class/drm")).unwrap(),
            Vec::new()
        );
    }
}
