use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::AsRawFd;

use crate::{Client, OfferId};

/// A caller-chosen stream name within one client, never reused after admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(NonZeroU64);

impl StreamId {
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

#[repr(C)]
#[derive(Default)]
struct CreateStream {
    id: u64,
    offer: u64,
    capacity: u32,
    flags: u32,
    reserved: u64,
}

#[repr(C)]
struct DestroyStream {
    id: u64,
    reserved: u64,
}

nix::ioctl_write_ptr!(create, b'd', 0x01, CreateStream);
nix::ioctl_write_ptr!(destroy, b'd', 0x02, DestroyStream);

impl Client {
    /// Open an offered configuration with independent request capacity.
    ///
    /// Use names greater than every previously admitted stream name. Failure
    /// consumes no name or storage. The kernel rechecks permission and the offer;
    /// a prior description query is not a reservation or authority for pixels.
    pub fn open_stream(
        &self,
        id: StreamId,
        offer: OfferId,
        capacity: NonZeroU32,
    ) -> io::Result<()> {
        let input = CreateStream {
            id: id.get(),
            offer: offer.get(),
            capacity: capacity.get(),
            ..Default::default()
        };
        // SAFETY: The complete initialized input remains readable through the call.
        unsafe { create(self.fd.as_raw_fd(), &input) }?;
        Ok(())
    }

    /// Stop admission and release a stream once its destination access has ended.
    ///
    /// EBUSY leaves the stream closing and requires a later retry; it is not
    /// successful cleanup. Revoked clients may still close streams. Success
    /// ends this stream's writes, not other users' access to the same buffers.
    /// No destructor invokes this operation or waits for storage to become idle.
    pub fn close_stream(&self, id: StreamId) -> io::Result<()> {
        let input = DestroyStream {
            id: id.get(),
            reserved: 0,
        };
        // SAFETY: Input is fully initialized and retained for the synchronous call.
        unsafe { destroy(self.fd.as_raw_fd(), &input) }?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn stream_names_exclude_zero_without_wrapping() {
        assert_eq!(StreamId::new(0), None);
        assert_eq!(StreamId::new(u64::MAX).unwrap().get(), u64::MAX);
    }

    #[test]
    fn stream_layout_matches_kernel_requests() {
        assert_eq!(size_of::<CreateStream>(), 32);
        assert_eq!(offset_of!(CreateStream, capacity), 16);
        assert_eq!(offset_of!(CreateStream, reserved), 24);
        assert_eq!(size_of::<DestroyStream>(), 16);
        assert_eq!(nix::request_code_write!(b'd', 1, 32), 0x4020_6401);
        assert_eq!(nix::request_code_write!(b'd', 2, 16), 0x4010_6402);
    }
}
