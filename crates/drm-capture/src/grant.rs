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
    let mut files = GrantFiles {
        capture: -1,
        control: -1,
    };
    let input = CreateGrant {
        crtc: crtc.get(),
        connector: connector.get(),
        files: (&mut files as *mut GrantFiles) as u64,
        ..Default::default()
    };
    // SAFETY: Input and separate writable output remain live through the ioctl.
    // Failure installs neither file, even if output memory was partially copied.
    unsafe { create(master.as_raw_fd(), &input) }?;
    if files.capture < 0 || files.control < 0 || files.capture == files.control {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid capture grant descriptors",
        ));
    }
    // SAFETY: Successful grant creation installs two fresh, distinct owned files.
    // Only this function receives those descriptors, and each is adopted once.
    let capture = unsafe { OwnedFd::from_raw_fd(files.capture) };
    // SAFETY: The second independently installed descriptor is also owned here.
    let control = unsafe { OwnedFd::from_raw_fd(files.control) };
    Ok((Client { fd: capture }, Control(control)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn grant_matches_the_kernel_layout() {
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
    }
}
