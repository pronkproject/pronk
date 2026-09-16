//! Retained capture authority before an output is ready to describe.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

use crate::{Client, Description};

/// A final-image capability, without monitor, renderer or revocation authority.
///
/// Retaining the descriptor does not keep its issuer alive or prevent revocation.
/// Opening a client checks the kernel's current authorization and active output.
#[derive(Debug)]
pub struct Access {
    fd: OwnedFd,
}

impl Access {
    /// Retain an issued descriptor without requiring an active display mode.
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self::from_fd(self.fd.try_clone()?))
    }

    pub fn open(&self) -> io::Result<Client> {
        Client::from_fd(self.fd.try_clone()?)
    }

    /// Observe current permission and output without duplicating the descriptor.
    ///
    /// The kernel validates the retained file on every query. No client, stream,
    /// destination, or reserved offer is created by a successful observation.
    pub fn describe(&self) -> io::Result<Description> {
        crate::description::query(self.fd.as_fd())
    }

    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn failed_validation_does_not_consume_retained_authority() {
        let (capability, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let access = Access::from_fd(capability.into());
        assert_eq!(
            access.open().unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        assert!(peer.read(&mut [0]).is_err());
        drop(access);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn clones_retain_only_the_same_file_description() {
        let (capability, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let access = Access::from_fd(capability.into());
        let duplicate = access.try_clone().unwrap();
        drop(access);
        assert!(peer.read(&mut [0]).is_err());
        drop(duplicate.into_fd());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn observation_failure_retains_the_file_for_another_query() {
        let (capability, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let access = Access::from_fd(capability.into());
        for _ in 0..2 {
            assert_eq!(
                access.describe().unwrap_err().raw_os_error(),
                Some(nix::libc::ENOTTY)
            );
        }
        assert!(peer.read(&mut [0]).is_err());
        drop(access);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
