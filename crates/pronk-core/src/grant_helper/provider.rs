//! Administrative CastKMS grant acquisition through a one-shot `pkexec`
//! helper.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

use async_trait::async_trait;
use castkms_sys::{
    drm_ioctl_castkms_get_grant, DrmCastkmsGetGrant, CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR,
    GRANT_FLAGS_MASK, GRANT_FLAG_ADMIN, GRANT_STATE_REVOKED, GRANT_STATE_SUSPENDED_FOREIGN_CONTENT,
};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::libc;
use nix::unistd::getuid;
use thiserror::Error;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::grant::{
    GrantAcquisitionError, GrantLease, GrantMetadata, GrantProfile, GrantProvider, GrantTarget,
    GrantValidationError,
};

use super::protocol::{
    CreateRequest, CreateResult, DiagnosticStage, Hello, Message, ProtocolError,
    HELPER_FEATURE_ADMIN_CONTROL_FD, MAX_MESSAGE_LENGTH, PROFILE_DISPLAY_CEC_AUDIO_V1,
    PROFILE_DISPLAY_CEC_V1, PROFILE_DISPLAY_V1,
};
use super::transport::{describe_exit, HelperExit, HelperTransport, TransportError};

pub const PKEXEC_PATH: &str = "/usr/bin/pkexec";
pub const GRANT_HELPER_PATH: &str = match option_env!("PRONK_GRANT_HELPER_PATH") {
    Some(path) => path,
    None => "/usr/libexec/pronk-grant-helper",
};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// System-mode provider backed by a short-lived privileged helper.
///
/// The long-lived daemon receives only a restricted holder and its anonymous
/// close-to-revoke control descriptor. The helper closes the privileged DRM
/// creator file before it exits.
#[derive(Debug, Default, Clone, Copy)]
pub struct PkexecAdminGrantProvider;

impl PkexecAdminGrantProvider {
    pub async fn acquire(&self, target: GrantTarget) -> Result<GrantLease, GrantProviderError> {
        let daemon_pid = std::process::id();
        let request_id = random_nonzero_u64()?;
        let mut command = Command::new(PKEXEC_PATH);
        command
            .arg("--disable-internal-agent")
            // ProtectHome hides /root from the service mount namespace. Keep
            // systemd's fixed `/` working directory instead of asking pkexec
            // to enter the target user's home before executing the helper.
            .arg("--keep-cwd")
            .arg(GRANT_HELPER_PATH);

        let transport = HelperTransport::spawn(command)?;
        let helper_pid = transport
            .child_id()
            .ok_or(GrantProviderError::MissingChildPid)?;

        let hello_datagram = match receive_with_timeout(&transport, "receive HELLO", 0).await {
            Ok(datagram) => datagram,
            Err(error) => {
                return Err(communication_failure(transport, "receive HELLO", error).await)
            }
        };
        let hello_message = Message::decode(&hello_datagram.payload)?;
        hello_message.validate_received_fd_count(hello_datagram.fds.len())?;
        let hello = match hello_message {
            Message::Hello(hello) => hello,
            _ => return Err(GrantProviderError::UnexpectedMessage("HELLO")),
        };
        validate_hello(&hello, helper_pid, daemon_pid, target.profile)?;

        let request = Message::CreateRequest {
            request_id,
            request: CreateRequest {
                expected_daemon_pid: daemon_pid,
                device_major: target.device_major,
                device_minor: target.device_minor,
                connector_id: target.connector_id,
                profile: target.profile,
            },
        };
        let encoded_request = request.encode()?;
        tokio::time::timeout(OPERATION_TIMEOUT, transport.send(&encoded_request))
            .await
            .map_err(|_| GrantProviderError::Timeout {
                operation: "send CREATE_REQUEST",
                duration: OPERATION_TIMEOUT,
            })??;

        let result_datagram = match receive_with_timeout(&transport, "receive CREATE_RESULT", 2)
            .await
        {
            Ok(datagram) => datagram,
            Err(error) => {
                return Err(communication_failure(transport, "receive CREATE_RESULT", error).await)
            }
        };
        let result_message = Message::decode(&result_datagram.payload)?;
        result_message.validate_received_fd_count(result_datagram.fds.len())?;
        let result = match result_message {
            Message::CreateResult {
                request_id: returned_request_id,
                result,
            } if returned_request_id == request_id => result,
            Message::CreateResult { .. } => return Err(GrantProviderError::WrongRequestId),
            _ => return Err(GrantProviderError::UnexpectedMessage("CREATE_RESULT")),
        };

        if result.status < 0 {
            let helper_exit = finish_helper(transport).await?;
            return Err(GrantProviderError::HelperRejected {
                status: result.status,
                stage: result.diagnostic_stage,
                exit: describe_exit(helper_exit.status),
                diagnostics: diagnostics_text(&helper_exit.diagnostics),
            });
        }

        validate_result(&result, &target)?;
        let mut descriptors = result_datagram.fds.into_iter();
        let holder = descriptors
            .next()
            .expect("successful result fd count was validated");
        let control = descriptors
            .next()
            .expect("successful result fd count was validated");
        debug_assert!(descriptors.next().is_none());

        let helper_exit = finish_helper(transport).await?;
        require_successful_exit(&helper_exit)?;
        let query = verify_holder_after_helper_exit(&holder, &result)?;
        verify_control_after_helper_exit(&control)?;

        GrantLease::from_administrator(
            holder,
            control,
            GrantMetadata {
                grant_id: query.grant_id,
                connector_id: query.connector_id,
                output_index: query.output_index,
                rights: query.rights,
                flags: query.flags,
                initial_state: query.state,
                capture_uapi_major: result.capture_uapi_major,
                capture_uapi_minor: result.capture_uapi_minor,
            },
            target.connector_id,
            target.profile.rights(),
        )
        .map_err(GrantProviderError::GrantValidation)
    }
}

#[async_trait]
impl GrantProvider for PkexecAdminGrantProvider {
    async fn acquire(
        &self,
        target: GrantTarget,
        cancellation: CancellationToken,
    ) -> Result<GrantLease, GrantAcquisitionError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(GrantAcquisitionError::Cancelled),
            result = PkexecAdminGrantProvider::acquire(self, target) => {
                result.map_err(GrantAcquisitionError::provider)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum GrantProviderError {
    #[error("grant-helper transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("grant-helper protocol failed: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("generate grant-helper request identity: {0}")]
    RequestIdentity(#[from] io::Error),
    #[error("spawned helper has no process ID")]
    MissingChildPid,
    #[error("timed out after {duration:?} while waiting to {operation}")]
    Timeout {
        operation: &'static str,
        duration: Duration,
    },
    #[error("expected {0} message from grant helper")]
    UnexpectedMessage(&'static str),
    #[error("grant-helper HELLO identity or capabilities differ: {0}")]
    InvalidHello(&'static str),
    #[error("grant helper returned a result for a different request")]
    WrongRequestId,
    #[error("grant-helper result metadata differs: {0}")]
    InvalidResult(&'static str),
    #[error(
        "grant helper rejected the request with {status} at {stage:?} ({exit}): {diagnostics}"
    )]
    HelperRejected {
        status: i32,
        stage: DiagnosticStage,
        exit: String,
        diagnostics: String,
    },
    #[error("grant helper terminated with {exit}: {diagnostics}")]
    HelperExit { exit: String, diagnostics: String },
    #[error("failed to {operation}: {cause}; grant helper terminated with {exit}: {diagnostics}")]
    HelperCommunication {
        operation: &'static str,
        cause: String,
        exit: String,
        diagnostics: String,
    },
    #[error("post-exit CastKMS grant query failed: {0}")]
    HolderQuery(Errno),
    #[error("post-exit CastKMS holder metadata differs: {0}")]
    InvalidHolder(&'static str),
    #[error("post-exit CastKMS control-fd inspection failed: {0}")]
    ControlQuery(Errno),
    #[error("post-exit CastKMS control fd differs: {0}")]
    InvalidControl(&'static str),
    #[error("validate transferred administrative grant: {0}")]
    GrantValidation(#[source] GrantValidationError),
}

async fn receive_with_timeout(
    transport: &HelperTransport,
    operation: &'static str,
    maximum_fd_count: usize,
) -> Result<super::transport::ReceivedDatagram, GrantProviderError> {
    tokio::time::timeout(
        OPERATION_TIMEOUT,
        transport.receive(MAX_MESSAGE_LENGTH, maximum_fd_count),
    )
    .await
    .map_err(|_| GrantProviderError::Timeout {
        operation,
        duration: OPERATION_TIMEOUT,
    })?
    .map_err(GrantProviderError::from)
}

async fn finish_helper(transport: HelperTransport) -> Result<HelperExit, GrantProviderError> {
    tokio::time::timeout(
        EXIT_TIMEOUT + Duration::from_secs(1),
        transport.finish(EXIT_TIMEOUT),
    )
    .await
    .map_err(|_| GrantProviderError::Timeout {
        operation: "wait for helper exit",
        duration: EXIT_TIMEOUT,
    })?
    .map_err(GrantProviderError::from)
}

async fn communication_failure(
    transport: HelperTransport,
    operation: &'static str,
    error: GrantProviderError,
) -> GrantProviderError {
    let cause = error.to_string();
    match finish_helper(transport).await {
        Ok(helper_exit) => GrantProviderError::HelperCommunication {
            operation,
            cause,
            exit: describe_exit(helper_exit.status),
            diagnostics: diagnostics_text(&helper_exit.diagnostics),
        },
        Err(finish_error) => GrantProviderError::HelperCommunication {
            operation,
            cause,
            exit: format!("exit status unavailable ({finish_error})"),
            diagnostics: String::new(),
        },
    }
}

fn validate_hello(
    hello: &Hello,
    helper_pid: u32,
    daemon_pid: u32,
    profile: GrantProfile,
) -> Result<(), GrantProviderError> {
    if hello.pkexec_uid != getuid().as_raw() {
        return Err(GrantProviderError::InvalidHello("PKEXEC_UID"));
    }
    if hello.helper_pid != helper_pid {
        return Err(GrantProviderError::InvalidHello("helper PID"));
    }
    if hello.parent_pid != daemon_pid {
        return Err(GrantProviderError::InvalidHello("parent PID"));
    }
    if hello.supported_profiles & profile_mask(profile) == 0 {
        return Err(GrantProviderError::InvalidHello("requested grant profile"));
    }
    if hello.helper_features != HELPER_FEATURE_ADMIN_CONTROL_FD {
        return Err(GrantProviderError::InvalidHello(
            "administrative lifetime-fd feature",
        ));
    }
    Ok(())
}

fn validate_result(result: &CreateResult, target: &GrantTarget) -> Result<(), GrantProviderError> {
    if result.grant_id == 0 {
        return Err(GrantProviderError::InvalidResult("zero grant ID"));
    }
    if result.connector_id != target.connector_id {
        return Err(GrantProviderError::InvalidResult("connector ID"));
    }
    if result.actual_rights != target.profile.rights() {
        return Err(GrantProviderError::InvalidResult("rights"));
    }
    if result.grant_flags & GRANT_FLAGS_MASK != GRANT_FLAG_ADMIN
        || result.grant_flags & !GRANT_FLAGS_MASK != 0
    {
        return Err(GrantProviderError::InvalidResult("grant flags"));
    }
    if result.initial_grant_state > GRANT_STATE_SUSPENDED_FOREIGN_CONTENT
        || result.initial_grant_state == GRANT_STATE_REVOKED
    {
        return Err(GrantProviderError::InvalidResult("grant state"));
    }
    if result.capture_uapi_major != CAPTURE_UAPI_MAJOR
        || result.capture_uapi_minor < CAPTURE_UAPI_MINOR
    {
        return Err(GrantProviderError::InvalidResult("capture UAPI version"));
    }
    if result.helper_features != HELPER_FEATURE_ADMIN_CONTROL_FD {
        return Err(GrantProviderError::InvalidResult("helper features"));
    }
    Ok(())
}

fn profile_mask(profile: GrantProfile) -> u32 {
    match profile {
        GrantProfile::DisplayV1 => PROFILE_DISPLAY_V1,
        GrantProfile::DisplayCecV1 => PROFILE_DISPLAY_CEC_V1,
        GrantProfile::DisplayCecAudioV1 => PROFILE_DISPLAY_CEC_AUDIO_V1,
    }
}

fn require_successful_exit(exit: &HelperExit) -> Result<(), GrantProviderError> {
    if exit.status.success() {
        Ok(())
    } else {
        Err(GrantProviderError::HelperExit {
            exit: describe_exit(exit.status),
            diagnostics: diagnostics_text(&exit.diagnostics),
        })
    }
}

fn verify_holder_after_helper_exit(
    holder: &OwnedFd,
    result: &CreateResult,
) -> Result<DrmCastkmsGetGrant, GrantProviderError> {
    let descriptor_flags =
        fcntl(holder.as_raw_fd(), FcntlArg::F_GETFD).map_err(GrantProviderError::HolderQuery)?;
    if !FdFlag::from_bits_truncate(descriptor_flags).contains(FdFlag::FD_CLOEXEC) {
        return Err(GrantProviderError::InvalidHolder("FD_CLOEXEC"));
    }
    let status_flags =
        fcntl(holder.as_raw_fd(), FcntlArg::F_GETFL).map_err(GrantProviderError::HolderQuery)?;
    if !OFlag::from_bits_truncate(status_flags).contains(OFlag::O_NONBLOCK) {
        return Err(GrantProviderError::InvalidHolder("O_NONBLOCK"));
    }

    let mut query = DrmCastkmsGetGrant::default();
    // SAFETY: `query` exactly matches the checked-in CastKMS UAPI layout and
    // remains writable for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_castkms_get_grant(holder.as_raw_fd(), &mut query) }
        .map_err(GrantProviderError::HolderQuery)?;
    if query.reserved != 0 {
        return Err(GrantProviderError::InvalidHolder("reserved field"));
    }
    if query.grant_id != result.grant_id
        || query.connector_id != result.connector_id
        || query.output_index != result.output_index
        || query.rights != result.actual_rights
        || query.flags != result.grant_flags
    {
        return Err(GrantProviderError::InvalidHolder("grant identity"));
    }
    if query.flags & GRANT_FLAGS_MASK != GRANT_FLAG_ADMIN || query.flags & !GRANT_FLAGS_MASK != 0 {
        return Err(GrantProviderError::InvalidHolder("grant flags"));
    }
    if query.state > GRANT_STATE_SUSPENDED_FOREIGN_CONTENT || query.state == GRANT_STATE_REVOKED {
        return Err(GrantProviderError::InvalidHolder("grant state"));
    }
    Ok(query)
}

fn verify_control_after_helper_exit(control: &OwnedFd) -> Result<(), GrantProviderError> {
    let descriptor_flags =
        fcntl(control.as_raw_fd(), FcntlArg::F_GETFD).map_err(GrantProviderError::ControlQuery)?;
    if !FdFlag::from_bits_truncate(descriptor_flags).contains(FdFlag::FD_CLOEXEC) {
        return Err(GrantProviderError::InvalidControl("FD_CLOEXEC"));
    }

    let mut poll_fd = libc::pollfd {
        fd: control.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    loop {
        poll_fd.revents = 0;
        // SAFETY: `poll_fd` names one initialized entry for this nonblocking poll.
        let result = unsafe { libc::poll(&mut poll_fd, 1, 0) };
        if result >= 0 {
            break;
        }
        let error = Errno::last();
        if error != Errno::EINTR {
            return Err(GrantProviderError::ControlQuery(error));
        }
    }
    if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(GrantProviderError::InvalidControl(
            "descriptor is already terminal",
        ));
    }
    Ok(())
}

fn random_nonzero_u64() -> Result<u64, io::Error> {
    loop {
        let mut value = 0_u64;
        // SAFETY: `value` is writable for exactly its size and getrandom does
        // not retain the pointer.
        let count = unsafe {
            libc::getrandom(
                (&mut value as *mut u64).cast(),
                std::mem::size_of::<u64>(),
                0,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count as usize != std::mem::size_of::<u64>() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned a short request ID",
            ));
        }
        if value != 0 {
            return Ok(value);
        }
    }
}

fn diagnostics_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    use castkms_sys::{DISPLAY_CEC_V1_RIGHTS, DISPLAY_V1_RIGHTS};

    use super::*;

    fn valid_result() -> CreateResult {
        CreateResult {
            status: 0,
            diagnostic_stage: DiagnosticStage::None,
            grant_id: 9,
            connector_id: 37,
            output_index: 2,
            actual_rights: DISPLAY_V1_RIGHTS,
            grant_flags: GRANT_FLAG_ADMIN,
            initial_grant_state: 1,
            capture_uapi_major: CAPTURE_UAPI_MAJOR,
            capture_uapi_minor: CAPTURE_UAPI_MINOR,
            helper_features: HELPER_FEATURE_ADMIN_CONTROL_FD,
        }
    }

    fn target() -> GrantTarget {
        GrantTarget {
            device_major: 226,
            device_minor: 9,
            connector_id: 37,
            profile: GrantProfile::DisplayV1,
        }
    }

    #[test]
    fn generates_nonzero_request_ids() {
        assert_ne!(random_nonzero_u64().unwrap(), 0);
    }

    #[test]
    fn result_validation_requires_the_fixed_profile() {
        let mut result = valid_result();
        assert!(validate_result(&result, &target()).is_ok());

        result.actual_rights |= 1 << 4;
        assert!(matches!(
            validate_result(&result, &target()),
            Err(GrantProviderError::InvalidResult("rights"))
        ));
        let mut cec_target = target();
        cec_target.profile = GrantProfile::DisplayCecV1;
        result.actual_rights = DISPLAY_CEC_V1_RIGHTS;
        assert!(validate_result(&result, &cec_target).is_ok());
    }

    #[test]
    fn control_validation_rejects_an_already_terminal_descriptor() {
        let (control, peer) = UnixStream::pair().unwrap();
        let control = OwnedFd::from(control);
        verify_control_after_helper_exit(&control).unwrap();

        drop(peer);
        assert!(matches!(
            verify_control_after_helper_exit(&control),
            Err(GrantProviderError::InvalidControl(
                "descriptor is already terminal"
            ))
        ));
    }
}
