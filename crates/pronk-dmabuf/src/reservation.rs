use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use crate::SyncFile;

/// The access performed by the submitting owner, not by its predecessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Access {
    Read = 1,
    Write = 2,
    ReadWrite = 3,
}

#[repr(C)]
struct Transfer {
    flags: u32,
    fd: i32,
}

nix::ioctl_readwrite!(export_sync_file, b'b', 2, Transfer);

/// Snapshot already-enrolled dependencies for the owner's next access.
///
/// Reads wait for writers; writes wait for readers and writers. The caller must
/// exclude other submissions from the snapshot through submission and import
/// of its completion. The ioctl cannot enforce that cross-process ownership.
/// Returned descriptors are close-on-exec, as required by the Linux ioctl.
pub fn export_dependencies(buffer: BorrowedFd<'_>, access: Access) -> io::Result<SyncFile> {
    let mut transfer = Transfer {
        flags: access as u32,
        fd: -1,
    };
    // SAFETY: The writable UAPI argument and borrowed descriptor remain valid
    // for the synchronous ioctl, which returns a newly owned descriptor.
    unsafe { export_sync_file(buffer.as_raw_fd(), &mut transfer) }?;
    if transfer.fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing exported sync file",
        ));
    }
    // SAFETY: A successful export returns a new descriptor owned by the caller.
    let fd = unsafe { OwnedFd::from_raw_fd(transfer.fd) };
    SyncFile::from_fd(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_layout_and_access_flags() {
        assert_eq!(std::mem::size_of::<Transfer>(), 8);
        assert_eq!(std::mem::offset_of!(Transfer, fd), 4);
        assert_eq!(Access::Read as u32, 1);
        assert_eq!(Access::Write as u32, 2);
        assert_eq!(Access::ReadWrite as u32, 3);
    }

    #[test]
    fn non_dma_buf_export_fails_without_taking_buffer_ownership() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(export_dependencies(file.as_fd(), Access::Write).is_err());
        assert!(file.metadata().is_ok());
    }
}
