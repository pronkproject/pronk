//! Fresh linear destinations from an explicitly selected DMA-BUF heap.
//!
//! This reference allocator does not select a GPU or promise encoder import.
//! Call it separately for each authorization domain; exported storage is not
//! revoked by closing a stream or assigning a different session identifier.

use std::fs::{File, OpenOptions};
use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::{invalid, Buffer, Layout};

#[repr(C)]
struct Allocation {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}

impl Allocation {
    fn new(len: u64) -> Self {
        Self {
            len,
            fd: 0,
            fd_flags: (nix::libc::O_CLOEXEC | nix::libc::O_RDWR) as u32,
            heap_flags: 0,
        }
    }
}

nix::ioctl_readwrite!(allocate, b'H', 0, Allocation);

pub struct Heap(File);

impl Heap {
    pub fn open(path: &Path) -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(Self)
    }

    /// Allocate independent buffers, checking the whole page-rounded pool budget
    /// before issuing any allocation. Failure drops all locally allocated files.
    pub fn allocate(
        &self,
        layout: Layout,
        count: NonZeroU32,
        max_bytes: NonZeroU64,
    ) -> io::Result<Vec<Buffer>> {
        // SAFETY: sysconf takes a constant selector and no borrowed memory.
        let page = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
        if page <= 0 {
            return Err(io::Error::other("cannot determine allocation page size"));
        }
        let (stride, len) = dimensions(layout, count, max_bytes, page as u64)?;
        let mut buffers = Vec::new();
        buffers
            .try_reserve_exact(count.get() as usize)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        for _ in 0..count.get() {
            let mut request = Allocation::new(len);
            // SAFETY: The initialized ABI structure is writable through the ioctl.
            // No fd is adopted on failure, regardless of modified output bytes.
            unsafe { allocate(self.0.as_raw_fd(), &mut request) }?;
            let fd = i32::try_from(request.fd).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid heap descriptor")
            })?;
            // SAFETY: Successful DMA_HEAP_IOCTL_ALLOC installs a fresh owned fd.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            buffers.push(Buffer::new_mappable(
                fd,
                stride,
                NonZeroU64::new(len).expect("page-rounded allocation is nonzero"),
            ));
        }
        Ok(buffers)
    }
}

fn dimensions(
    layout: Layout,
    count: NonZeroU32,
    max_bytes: NonZeroU64,
    page: u64,
) -> io::Result<(NonZeroU32, u64)> {
    if count.get() > 64 || !page.is_power_of_two() {
        return Err(invalid("invalid capture allocation count or page size"));
    }
    let stride = layout
        .width
        .get()
        .checked_mul(4)
        .and_then(NonZeroU32::new)
        .ok_or_else(|| invalid("capture stride exceeds the layout interface"))?;
    let bytes = u64::from(stride.get()) * u64::from(layout.height.get());
    let len = bytes
        .checked_add(page - 1)
        .map(|bytes| bytes & !(page - 1))
        .ok_or_else(|| invalid("capture allocation size overflow"))?;
    let total = len
        .checked_mul(u64::from(count.get()))
        .ok_or_else(|| invalid("capture pool size overflow"))?;
    if total > max_bytes.get() || usize::try_from(len).is_err() {
        return Err(invalid("capture pool exceeds its allocation budget"));
    }
    Ok((stride, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: u32, height: u32) -> Layout {
        Layout {
            width: NonZeroU32::new(width).unwrap(),
            height: NonZeroU32::new(height).unwrap(),
        }
    }

    #[test]
    fn heap_request_matches_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<Allocation>(), 24);
        assert_eq!(std::mem::offset_of!(Allocation, heap_flags), 16);
        assert_eq!(nix::request_code_readwrite!(b'H', 0, 24), 0xc018_4800);
        let request = Allocation::new(4096);
        assert_eq!(request.len, 4096);
        assert_eq!(request.fd, 0);
        assert_eq!(request.heap_flags, 0);
        assert_eq!(
            request.fd_flags,
            (nix::libc::O_RDWR | nix::libc::O_CLOEXEC) as u32
        );
    }

    #[test]
    fn budget_includes_page_rounding_for_every_buffer() {
        let count = NonZeroU32::new(3).unwrap();
        let exact = NonZeroU64::new(3 * 4096).unwrap();
        assert_eq!(
            dimensions(layout(17, 1), count, exact, 4096).unwrap(),
            (NonZeroU32::new(68).unwrap(), 4096)
        );
        assert!(dimensions(
            layout(17, 1),
            count,
            NonZeroU64::new(exact.get() - 1).unwrap(),
            4096
        )
        .is_err());
    }

    #[test]
    fn oversized_layouts_are_rejected_before_allocation() {
        let count = NonZeroU32::new(1).unwrap();
        let budget = NonZeroU64::new(u64::MAX).unwrap();
        assert!(dimensions(layout(u32::MAX, 1), count, budget, 4096).is_err());
        assert!(dimensions(layout(1, 1), NonZeroU32::new(65).unwrap(), budget, 4096).is_err());
        assert!(dimensions(layout(1, 1), count, budget, 3).is_err());
    }

    #[test]
    fn unrelated_file_is_not_an_allocator() {
        let heap = Heap::open(Path::new("/dev/null")).unwrap();
        let error = heap
            .allocate(
                layout(1, 1),
                NonZeroU32::new(1).unwrap(),
                NonZeroU64::new(4096).unwrap(),
            )
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ENOTTY));
        assert!(heap.0.metadata().is_ok());
    }
}
