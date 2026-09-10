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
nix::ioctl_write_ptr!(import_sync_file, b'b', 3, Transfer);

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

/// Enroll actual submitted completion for later implicitly synchronized users.
///
/// Failure does not cancel submitted work. Keep the allocation unavailable to
/// consumers until another native completion path establishes safe access.
/// The borrowed fence remains owned by the caller on success and failure.
pub fn import_completion(
    buffer: BorrowedFd<'_>,
    access: Access,
    completion: &SyncFile,
) -> io::Result<()> {
    let transfer = Transfer {
        flags: access as u32,
        fd: completion.as_fd().as_raw_fd(),
    };
    // SAFETY: Both borrowed descriptors and the initialized UAPI argument live
    // through the ioctl; the kernel takes its own reference to the fence.
    unsafe { import_sync_file(buffer.as_raw_fd(), &transfer) }?;
    Ok(())
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
