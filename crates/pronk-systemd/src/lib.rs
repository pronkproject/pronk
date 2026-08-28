//! Narrow systemd activation and readiness boundary.
//!
//! Activation descriptors must be consumed from a synchronous `main` before a
//! Tokio runtime or any other thread is created.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::sys::socket::{getpeername, getsockname, getsockopt, sockopt, SockType, UnixAddr};
use sd_notify::NotifyState;
use thiserror::Error;

mod peer;

pub use peer::*;

pub const BACKEND_CONTROL_FD_NAME: &str = "pronk-backend-control";

#[derive(Debug)]
pub struct BackendControlFd(OwnedFd);

impl BackendControlFd {
    pub fn into_std_stream(self) -> UnixStream {
        self.0.into()
    }
}

pub fn take_backend_control_fd() -> Result<BackendControlFd, ActivationError> {
    // SAFETY: This function is documented and intended to run synchronously at
    // the start of main, before Tokio or any other thread is created.
    let descriptors = unsafe { take_named_descriptors()? };
    validate_backend_control_descriptors(descriptors)
}

pub fn notify_ready() -> Result<(), io::Error> {
    sd_notify::notify(&[NotifyState::Ready])
}

pub fn notify_stopping() -> Result<(), io::Error> {
    sd_notify::notify(&[NotifyState::Stopping])
}

#[derive(Debug, Error)]
pub enum ActivationError {
    #[error("cannot consume systemd activation descriptors: {0}")]
    Intake(#[from] io::Error),
    #[error("expected one activation descriptor, received {0}")]
    WrongDescriptorCount(usize),
    #[error("expected activation descriptor name {expected:?}, received {actual:?}")]
    WrongDescriptorName {
        expected: &'static str,
        actual: String,
    },
    #[error("activation descriptor is not an AF_UNIX SOCK_STREAM: {0:?}")]
    WrongSocketType(SockType),
    #[error("activation descriptor is a listening socket")]
    ListeningSocket,
    #[error("activation descriptor is not a connected Unix socket: {0}")]
    NotConnected(nix::Error),
    #[error("activation descriptor does not have FD_CLOEXEC")]
    MissingCloseOnExec,
    #[error("cannot inspect or configure activation descriptor: {0}")]
    Descriptor(#[from] nix::Error),
}

#[derive(Debug)]
struct NamedDescriptor {
    name: String,
    fd: OwnedFd,
}

unsafe fn take_named_descriptors() -> Result<Vec<NamedDescriptor>, io::Error> {
    // SAFETY: The caller guarantees single-threaded startup, as required by
    // sd-notify's environment-consuming API.
    let descriptors = unsafe { sd_notify::listen_fds_with_names_and_unset_env()? };
    Ok(descriptors
        .map(|(raw_fd, name)| NamedDescriptor {
            name,
            // SAFETY: matching LISTEN_FDS descriptors are transferred to this
            // process by systemd and have not been wrapped elsewhere.
            fd: unsafe { OwnedFd::from_raw_fd(raw_fd) },
        })
        .collect())
}

fn validate_backend_control_descriptors(
    mut descriptors: Vec<NamedDescriptor>,
) -> Result<BackendControlFd, ActivationError> {
    if descriptors.len() != 1 {
        return Err(ActivationError::WrongDescriptorCount(descriptors.len()));
    }
    let descriptor = descriptors.pop().expect("length checked above");
    if descriptor.name != BACKEND_CONTROL_FD_NAME {
        return Err(ActivationError::WrongDescriptorName {
            expected: BACKEND_CONTROL_FD_NAME,
            actual: descriptor.name,
        });
    }

    validate_connected_unix_stream(&descriptor.fd)?;
    set_nonblocking(&descriptor.fd)?;
    Ok(BackendControlFd(descriptor.fd))
}

fn validate_connected_unix_stream(fd: &OwnedFd) -> Result<(), ActivationError> {
    let socket_type = getsockopt(fd, sockopt::SockType)?;
    if socket_type != SockType::Stream {
        return Err(ActivationError::WrongSocketType(socket_type));
    }
    if getsockopt(fd, sockopt::AcceptConn)? {
        return Err(ActivationError::ListeningSocket);
    }

    let raw_fd = fd.as_raw_fd();
    getsockname::<UnixAddr>(raw_fd).map_err(ActivationError::NotConnected)?;
    getpeername::<UnixAddr>(raw_fd).map_err(ActivationError::NotConnected)?;

    let flags = descriptor_flags(raw_fd)?;
    if !flags.contains(FdFlag::FD_CLOEXEC) {
        return Err(ActivationError::MissingCloseOnExec);
    }
    Ok(())
}

fn set_nonblocking(fd: &OwnedFd) -> Result<(), ActivationError> {
    let raw_fd = fd.as_raw_fd();
    let flags = OFlag::from_bits_truncate(fcntl(raw_fd, FcntlArg::F_GETFL)?);
    fcntl(raw_fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

fn descriptor_flags(fd: RawFd) -> Result<FdFlag, nix::Error> {
    Ok(FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD)?))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};

    use super::*;

    fn named(fd: OwnedFd, name: &str) -> NamedDescriptor {
        NamedDescriptor {
            name: name.to_owned(),
            fd,
        }
    }

    #[test]
    fn accepts_one_named_connected_unix_stream() {
        let (left, _right) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let control =
            validate_backend_control_descriptors(vec![named(left, BACKEND_CONTROL_FD_NAME)])
                .unwrap();

        let raw_flags = fcntl(control.0.as_raw_fd(), FcntlArg::F_GETFL).unwrap();
        assert!(OFlag::from_bits_truncate(raw_flags).contains(OFlag::O_NONBLOCK));
        let raw_fd_flags = fcntl(control.0.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
        assert!(FdFlag::from_bits_truncate(raw_fd_flags).contains(FdFlag::FD_CLOEXEC));
    }

    #[test]
    fn rejects_wrong_count_and_name() {
        assert!(matches!(
            validate_backend_control_descriptors(vec![]),
            Err(ActivationError::WrongDescriptorCount(0))
        ));

        let (left, _right) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        assert!(matches!(
            validate_backend_control_descriptors(vec![named(left, "wrong")]),
            Err(ActivationError::WrongDescriptorName { .. })
        ));
    }

    #[test]
    fn rejects_seqpacket_and_missing_cloexec() {
        let (seqpacket, _right) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        assert!(matches!(
            validate_backend_control_descriptors(vec![named(seqpacket, BACKEND_CONTROL_FD_NAME,)]),
            Err(ActivationError::WrongSocketType(SockType::SeqPacket))
        ));

        let (stream, _right) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )
        .unwrap();
        assert!(matches!(
            validate_backend_control_descriptors(vec![named(stream, BACKEND_CONTROL_FD_NAME,)]),
            Err(ActivationError::MissingCloseOnExec)
        ));
    }
}
