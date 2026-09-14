use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, BorrowedFd};

use crate::Client;

/// A destination registration name, separate from stream and request names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DestinationId(NonZeroU64);

impl DestinationId {
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// One plane within caller-owned DMA-BUF storage.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    pub buffer: BorrowedFd<'a>,
    pub stride: NonZeroU32,
    pub offset: u64,
}

/// Borrowed registration metadata; allocation and downstream reuse remain external.
#[derive(Debug)]
pub struct Destination<'a> {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub format: u32,
    pub modifier: u64,
    pub planes: &'a [Plane<'a>],
}

#[repr(C)]
#[derive(Default)]
struct Register {
    id: u64,
    width: u32,
    height: u32,
    format: u32,
    num_planes: u32,
    modifier: u64,
    fds: [i32; 4],
    strides: [u32; 4],
    offsets: [u64; 4],
    flags: u32,
    reserved: [u32; 3],
}

impl Register {
    fn new(id: DestinationId, image: &Destination<'_>) -> io::Result<Self> {
        if !(1..=4).contains(&image.planes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture requires one through four image planes",
            ));
        }
        let mut input = Self {
            id: id.get(),
            width: image.width.get(),
            height: image.height.get(),
            format: image.format,
            modifier: image.modifier,
            num_planes: image.planes.len() as u32,
            ..Default::default()
        };
        for (index, plane) in image.planes.iter().enumerate() {
            input.fds[index] = plane.buffer.as_raw_fd();
            input.strides[index] = plane.stride.get();
            input.offsets[index] = plane.offset;
        }
        Ok(input)
    }
}

#[repr(C)]
struct Unregister {
    id: u64,
    reserved: u64,
}

nix::ioctl_write_ptr!(register, b'd', 0x03, Register);
nix::ioctl_write_ptr!(unregister, b'd', 0x04, Unregister);

impl Client {
    /// Retain checked storage under a strictly increasing registration name.
    ///
    /// Kernel admission validates layout and write access. Success retains its
    /// own DMA-BUF references, not the submitted descriptor numbers. Neither
    /// registration nor a different name establishes exclusive access or proves
    /// independence from compositor storage. Allocation remains outside this client.
    pub fn register_destination(
        &self,
        id: DestinationId,
        image: &Destination<'_>,
    ) -> io::Result<()> {
        let input = Register::new(id, image)?;
        // SAFETY: Input has no pointers; all borrowed DMA-BUF descriptors remain
        // live through the synchronous call. Inactive entries and flags are zero.
        unsafe { register(self.fd.as_raw_fd(), &input) }?;
        Ok(())
    }

    /// Remove a name without cancelling accepted writes or acknowledging reuse.
    ///
    /// The name cannot be reused. Removal remains available after revocation;
    /// pending requests retain their exact destination independently.
    pub fn unregister_destination(&self, id: DestinationId) -> io::Result<()> {
        let input = Unregister {
            id: id.get(),
            reserved: 0,
        };
        // SAFETY: The initialized input remains live through the call.
        unsafe { unregister(self.fd.as_raw_fd(), &input) }?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use std::os::fd::AsFd;

    #[test]
    fn destination_layout_matches_the_kernel() {
        assert_eq!(size_of::<Register>(), 112);
        assert_eq!(offset_of!(Register, fds), 32);
        assert_eq!(offset_of!(Register, offsets), 64);
        assert_eq!(offset_of!(Register, reserved), 100);
        assert_eq!(size_of::<Unregister>(), 16);
        assert_eq!(nix::request_code_write!(b'd', 3, 112), 0x4070_6403);
        assert_eq!(nix::request_code_write!(b'd', 4, 16), 0x4010_6404);
        assert!(DestinationId::new(0).is_none());
    }

    #[test]
    fn registration_preserves_active_planes_and_zeros_unused_entries() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let plane = Plane {
            buffer: file.as_fd(),
            stride: NonZeroU32::new(2560).unwrap(),
            offset: 64,
        };
        let planes = [plane; 5];
        for count in 0..=5 {
            let image = Destination {
                width: NonZeroU32::new(640).unwrap(),
                height: NonZeroU32::new(480).unwrap(),
                format: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
                planes: &planes[..count],
            };
            let input = Register::new(DestinationId::new(11).unwrap(), &image);
            if count == 0 || count == 5 {
                assert!(input.is_err());
                continue;
            }
            let input = input.unwrap();
            assert_eq!(input.num_planes, count as u32);
            assert_eq!(input.id, 11);
            for i in 0..4 {
                assert_eq!(input.fds[i], if i < count { file.as_raw_fd() } else { 0 });
                assert_eq!(input.strides[i], if i < count { 2560 } else { 0 });
                assert_eq!(input.offsets[i], if i < count { 64 } else { 0 });
            }
            assert_eq!(input.flags, 0);
            assert_eq!(input.reserved, [0; 3]);
        }
    }
}
