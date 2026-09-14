use std::io;
use std::os::fd::{AsFd, AsRawFd};

use crate::{Client, RequestId, StreamId};

#[repr(C)]
struct Cancel {
    stream: u64,
    request: u64,
    reserved: u64,
}

nix::ioctl_write_ptr!(cancel, b'd', 0x07, Cancel);

impl<F: AsFd> Client<F> {
    /// Request cancellation, without releasing request credit or acknowledging reuse.
    ///
    /// Observe terminal completion or successfully close the stream before
    /// reusing its destination. EALREADY preserves an existing cancellation or
    /// terminal result, and ENOENT means no such request remains in the stream.
    /// This cleanup operation remains available after capture authority is revoked.
    pub fn cancel(&self, stream: StreamId, request: RequestId) -> io::Result<()> {
        let input = Cancel {
            stream: stream.get(),
            request: request.get(),
            reserved: 0,
        };
        // SAFETY: Input is initialized and readable for the synchronous call.
        unsafe { cancel(self.as_fd().as_raw_fd(), &input) }?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn cancellation_layout_matches_the_kernel() {
        assert_eq!(size_of::<Cancel>(), 24);
        assert_eq!(offset_of!(Cancel, request), 8);
        assert_eq!(offset_of!(Cancel, reserved), 16);
        assert_eq!(nix::request_code_write!(b'd', 7, 24), 0x4018_6407);
    }
}
