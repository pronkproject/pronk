//! Ownership and validation of compositor-issued CastKMS grants.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use async_trait::async_trait;
use castkms_sys::{
    drm_ioctl_castkms_get_grant, DrmCastkmsGetGrant, CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR,
    GRANT_STATE_REVOKED, GRANT_STATE_SUSPENDED_FOREIGN_CONTENT,
};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Fixed rights profiles understood by Pronk and Mutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum GrantProfile {
    DisplayV1 = 1,
    DisplayCecV1 = 2,
}

impl GrantProfile {
    pub const fn rights(self) -> u32 {
        match self {
            Self::DisplayV1 => castkms_sys::DISPLAY_V1_RIGHTS,
            Self::DisplayCecV1 => castkms_sys::DISPLAY_CEC_V1_RIGHTS,
        }
    }
}

/// Connector identity required to acquire one restricted grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantTarget {
    pub device_major: u32,
    pub device_minor: u32,
    pub connector_id: u32,
    pub profile: GrantProfile,
}

/// Dyn-safe provider boundary for one connector-scoped restricted capability.
#[async_trait]
pub trait GrantProvider: std::fmt::Debug + Send + Sync + 'static {
    async fn acquire(
        &self,
        target: GrantTarget,
        cancellation: CancellationToken,
    ) -> Result<GrantLease, GrantAcquisitionError>;
}

#[derive(Debug, Error)]
pub enum GrantAcquisitionError {
    #[error("grant acquisition was cancelled")]
    Cancelled,
    #[error("grant provider failed: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl GrantAcquisitionError {
    pub fn provider(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Provider(Box::new(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantMetadata {
    pub grant_id: u32,
    pub connector_id: u32,
    pub output_index: u32,
    pub rights: u32,
    pub flags: u32,
    pub initial_state: u32,
    pub capture_uapi_major: u16,
    pub capture_uapi_minor: u16,
}

/// A validated holder descriptor for a grant retained by the compositor.
#[derive(Debug)]
pub struct GrantLease {
    holder: OwnedFd,
    metadata: GrantMetadata,
}

#[derive(Debug, Error)]
pub enum GrantValidationError {
    #[error("compositor grant metadata differs: {0}")]
    InvalidMetadata(&'static str),
    #[error("query compositor grant descriptor: {0}")]
    HolderQuery(#[source] Errno),
    #[error("compositor grant descriptor differs: {0}")]
    InvalidHolder(&'static str),
}

impl GrantLease {
    #[cfg(test)]
    pub(crate) fn new_unchecked(holder: OwnedFd, metadata: GrantMetadata) -> Self {
        Self { holder, metadata }
    }

    /// Validate and own a grant whose private control endpoint remains with
    /// the compositor that issued it.
    pub fn from_compositor(
        holder: OwnedFd,
        metadata: GrantMetadata,
        expected_connector_id: u32,
        expected_rights: u32,
    ) -> Result<Self, GrantValidationError> {
        validate_metadata(&metadata, expected_connector_id, expected_rights)?;
        validate_holder(&holder, &metadata)?;
        Ok(Self { holder, metadata })
    }

    /// Borrow the restricted holder used for every CastKMS operation.
    pub fn holder(&self) -> BorrowedFd<'_> {
        self.holder.as_fd()
    }

    pub fn grant_id(&self) -> u32 {
        self.metadata.grant_id
    }

    pub fn connector_id(&self) -> u32 {
        self.metadata.connector_id
    }

    pub fn output_index(&self) -> u32 {
        self.metadata.output_index
    }

    pub fn rights(&self) -> u32 {
        self.metadata.rights
    }

    pub fn flags(&self) -> u32 {
        self.metadata.flags
    }

    pub fn capture_uapi_major(&self) -> u16 {
        self.metadata.capture_uapi_major
    }

    pub fn capture_uapi_minor(&self) -> u16 {
        self.metadata.capture_uapi_minor
    }
}

fn validate_metadata(
    metadata: &GrantMetadata,
    expected_connector_id: u32,
    expected_rights: u32,
) -> Result<(), GrantValidationError> {
    if metadata.grant_id == 0 {
        return Err(GrantValidationError::InvalidMetadata("zero grant ID"));
    }
    if metadata.connector_id != expected_connector_id {
        return Err(GrantValidationError::InvalidMetadata("connector ID"));
    }
    if metadata.rights != expected_rights {
        return Err(GrantValidationError::InvalidMetadata("rights"));
    }
    if metadata.flags != 0 {
        return Err(GrantValidationError::InvalidMetadata("grant flags"));
    }
    if !is_live_grant_state(metadata.initial_state) {
        return Err(GrantValidationError::InvalidMetadata("initial grant state"));
    }
    if metadata.capture_uapi_major != CAPTURE_UAPI_MAJOR
        || metadata.capture_uapi_minor < CAPTURE_UAPI_MINOR
    {
        return Err(GrantValidationError::InvalidMetadata(
            "capture UAPI version",
        ));
    }
    Ok(())
}

fn validate_holder(holder: &OwnedFd, metadata: &GrantMetadata) -> Result<(), GrantValidationError> {
    let descriptor_flags =
        fcntl(holder.as_raw_fd(), FcntlArg::F_GETFD).map_err(GrantValidationError::HolderQuery)?;
    if !FdFlag::from_bits_truncate(descriptor_flags).contains(FdFlag::FD_CLOEXEC) {
        return Err(GrantValidationError::InvalidHolder("FD_CLOEXEC"));
    }
    let status_flags =
        fcntl(holder.as_raw_fd(), FcntlArg::F_GETFL).map_err(GrantValidationError::HolderQuery)?;
    if !OFlag::from_bits_truncate(status_flags).contains(OFlag::O_NONBLOCK) {
        return Err(GrantValidationError::InvalidHolder("O_NONBLOCK"));
    }

    let mut query = DrmCastkmsGetGrant::default();
    // SAFETY: `query` exactly matches the checked-in CastKMS UAPI layout and
    // remains writable for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_castkms_get_grant(holder.as_raw_fd(), &mut query) }
        .map_err(GrantValidationError::HolderQuery)?;
    if query.reserved != 0 {
        return Err(GrantValidationError::InvalidHolder("reserved field"));
    }
    if query.grant_id != metadata.grant_id
        || query.connector_id != metadata.connector_id
        || query.output_index != metadata.output_index
        || query.rights != metadata.rights
        || query.flags != metadata.flags
    {
        return Err(GrantValidationError::InvalidHolder("grant identity"));
    }
    if query.flags != 0 {
        return Err(GrantValidationError::InvalidHolder("grant flags"));
    }
    if !is_live_grant_state(query.state) {
        return Err(GrantValidationError::InvalidHolder("grant state"));
    }
    Ok(())
}

fn is_live_grant_state(state: u32) -> bool {
    state <= GRANT_STATE_SUSPENDED_FOREIGN_CONTENT && state != GRANT_STATE_REVOKED
}

#[cfg(test)]
mod tests {
    use castkms_sys::{
        CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR, DISPLAY_V1_RIGHTS, GRANT_STATE_ACTIVE,
    };

    use super::*;

    #[test]
    fn metadata_is_exactly_connector_profile_and_uapi_scoped() {
        let metadata = valid_metadata();
        validate_metadata(&metadata, 43, DISPLAY_V1_RIGHTS).unwrap();

        for (invalid, field) in [
            (
                GrantMetadata {
                    grant_id: 0,
                    ..metadata
                },
                "zero grant ID",
            ),
            (
                GrantMetadata {
                    connector_id: 44,
                    ..metadata
                },
                "connector ID",
            ),
            (
                GrantMetadata {
                    rights: DISPLAY_V1_RIGHTS ^ 1,
                    ..metadata
                },
                "rights",
            ),
            (
                GrantMetadata {
                    flags: 1,
                    ..metadata
                },
                "grant flags",
            ),
            (
                GrantMetadata {
                    initial_state: GRANT_STATE_REVOKED,
                    ..metadata
                },
                "initial grant state",
            ),
            (
                GrantMetadata {
                    capture_uapi_minor: CAPTURE_UAPI_MINOR - 1,
                    ..metadata
                },
                "capture UAPI version",
            ),
        ] {
            assert!(matches!(
                validate_metadata(&invalid, 43, DISPLAY_V1_RIGHTS),
                Err(GrantValidationError::InvalidMetadata(actual)) if actual == field
            ));
        }
    }

    fn valid_metadata() -> GrantMetadata {
        GrantMetadata {
            grant_id: 1,
            connector_id: 43,
            output_index: 3,
            rights: DISPLAY_V1_RIGHTS,
            flags: 0,
            initial_state: GRANT_STATE_ACTIVE,
            capture_uapi_major: CAPTURE_UAPI_MAJOR,
            capture_uapi_minor: CAPTURE_UAPI_MINOR,
        }
    }
}
