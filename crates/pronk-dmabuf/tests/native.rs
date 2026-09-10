//! Opt-in kernel ioctl checks, independent of a graphics or capture driver.
//! Empty reservation fences do not exercise pending GPU work or GPU errors.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use pronk_dmabuf::{export_dependencies, import_completion, Access, Completion};

#[repr(C)]
struct Allocation {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}

nix::ioctl_readwrite!(allocate, b'H', 0, Allocation);

#[tokio::test]
#[ignore = "requires access to /dev/dma_heap/system"]
async fn empty_reservation_roundtrip() {
    let heap = std::fs::File::open("/dev/dma_heap/system").unwrap();
    let mut allocation = Allocation {
        len: 4096,
        fd: 0,
        fd_flags: (nix::libc::O_RDWR | nix::libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    // SAFETY: The writable allocation request has the Linux DMA heap layout
    // and lives through the ioctl, which returns a newly owned descriptor.
    unsafe { allocate(heap.as_raw_fd(), &mut allocation) }.unwrap();
    let raw = i32::try_from(allocation.fd).unwrap();
    // SAFETY: The successful allocation returned a new descriptor to own.
    let buffer = unsafe { OwnedFd::from_raw_fd(raw) };
    for access in [Access::Read, Access::Write, Access::ReadWrite] {
        let fence = export_dependencies(buffer.as_fd(), access).unwrap();
        assert_eq!(fence.completion().unwrap(), Some(Completion::Success));
        let flags = fcntl(fence.as_fd().as_raw_fd(), FcntlArg::F_GETFD).unwrap();
        assert!(FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC));
        let invalid_buffer = std::fs::File::open("/dev/null").unwrap();
        assert!(import_completion(invalid_buffer.as_fd(), access, &fence).is_err());
        import_completion(buffer.as_fd(), access, &fence).unwrap();
        assert_eq!(fence.wait().await.unwrap(), Completion::Success);
        assert_eq!(
            export_dependencies(buffer.as_fd(), Access::ReadWrite)
                .unwrap()
                .wait()
                .await
                .unwrap(),
            Completion::Success,
        );
    }
}
