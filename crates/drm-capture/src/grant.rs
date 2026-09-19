use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use crate::Client;

/// Revocation authority without pixel or modesetting access.
///
/// Dropping the last reference to this file revokes its capture grant. Closing
/// the issuing DRM file also revokes it, even if this control remains alive.
#[derive(Debug)]
pub struct Control(OwnedFd);

impl AsFd for Control {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[repr(C)]
struct GrantFiles {
    capture: i32,
    control: i32,
}

#[repr(C)]
#[derive(Default)]
struct CreateGrant {
    crtc: u32,
    connector: u32,
    files: u64,
    flags: u32,
    reserved: [u32; 3],
}

nix::ioctl_write_ptr!(create, b'd', 0xd4, CreateGrant);

const CREATE_ADMIN: u32 = 1 << 0;

/// Issue a creator-bound grant for an exact output on the current DRM master.
///
/// The kernel validates the target and provider support. Both returned files
/// are close-on-exec. Issuance does not authorize all future content, capture
/// pixels or allocate image storage. Keep display-control ownership separate.
pub fn create_grant(
    master: BorrowedFd<'_>,
    crtc: NonZeroU32,
    connector: NonZeroU32,
) -> io::Result<(Client, Control)> {
    create_grant_with_flags(master, crtc, connector, 0)
}

/// Issue a grant through a privileged, non-master DRM file.
///
/// The kernel requires `CAP_SYS_ADMIN` in the initial user namespace, provider
/// opt-in and a distinct current DRM master. The grant is bound to that owner's
/// current interval and becomes stale when the interval ends. This function
/// does not acquire DRM master or confer any modesetting authority.
pub fn create_administrative_grant(
    issuer: BorrowedFd<'_>,
    crtc: NonZeroU32,
    connector: NonZeroU32,
) -> io::Result<(Client, Control)> {
    create_grant_with_flags(issuer, crtc, connector, CREATE_ADMIN)
}

fn create_grant_with_flags(
    issuer: BorrowedFd<'_>,
    crtc: NonZeroU32,
    connector: NonZeroU32,
    flags: u32,
) -> io::Result<(Client, Control)> {
    let mut files = GrantFiles {
        capture: -1,
        control: -1,
    };
    let input = CreateGrant {
        crtc: crtc.get(),
        connector: connector.get(),
        files: (&mut files as *mut GrantFiles) as u64,
        flags,
        ..Default::default()
    };
    // SAFETY: Input and separate writable output remain live through the ioctl.
    // Failure installs neither file, even if output memory was partially copied.
    unsafe { create(issuer.as_raw_fd(), &input) }?;
    adopt_grant_files(files)
}

fn adopt_grant_files(files: GrantFiles) -> io::Result<(Client, Control)> {
    // SAFETY: Successful publication installs fresh owned descriptors. Adopt
    // each nonnegative number once so malformed success still closes it.
    let capture = (files.capture >= 0).then(|| unsafe { OwnedFd::from_raw_fd(files.capture) });
    let control = (files.control >= 0 && files.control != files.capture)
        .then(|| unsafe { OwnedFd::from_raw_fd(files.control) });
    let (Some(capture), Some(control)) = (capture, control) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid capture grant descriptors",
        ));
    };
    Ok((Client { fd: capture }, Control(control)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use std::os::fd::IntoRawFd;

    #[test]
    fn grant_matches_the_kernel_layout() {
        assert_eq!(CREATE_ADMIN, 1);
        assert_eq!(size_of::<GrantFiles>(), 8);
        assert_eq!(size_of::<CreateGrant>(), 32);
        assert_eq!(offset_of!(CreateGrant, files), 8);
        assert_eq!(offset_of!(CreateGrant, reserved), 20);
        assert_eq!(nix::request_code_write!(b'd', 0xd4, 32), 0x4020_64d4);
    }

    #[test]
    fn failed_issuance_does_not_adopt_output() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let id = NonZeroU32::new(1).unwrap();
        assert_eq!(
            create_grant(file.as_fd(), id, id)
                .unwrap_err()
                .raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        assert!(file.metadata().is_ok());
        assert_eq!(
            create_administrative_grant(file.as_fd(), id, id)
                .unwrap_err()
                .raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
    }

    #[test]
    fn malformed_success_closes_each_installed_descriptor_once() {
        let (first, second) = nix::unistd::pipe().unwrap();
        let first = first.into_raw_fd();
        let second = second.into_raw_fd();
        assert!(adopt_grant_files(GrantFiles {
            capture: first,
            control: first,
        })
        .is_err());
        assert_eq!(
            nix::fcntl::fcntl(first, nix::fcntl::FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );
        assert!(adopt_grant_files(GrantFiles {
            capture: -1,
            control: second,
        })
        .is_err());
        assert_eq!(
            nix::fcntl::fcntl(second, nix::fcntl::FcntlArg::F_GETFD),
            Err(nix::errno::Errno::EBADF)
        );
    }
}
