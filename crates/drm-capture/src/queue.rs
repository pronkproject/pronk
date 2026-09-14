use std::io;
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use crate::{Client, DestinationId, StreamId};

/// A strictly increasing request name within one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(NonZeroU64);

impl RequestId {
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

#[repr(C)]
#[derive(Default)]
struct Queue {
    stream: u64,
    request: u64,
    destination: u64,
    reuse_fd: i32,
    flags: u32,
    reserved: u64,
}

impl Queue {
    fn new(
        stream: StreamId,
        request: RequestId,
        destination: DestinationId,
        reuse: Option<BorrowedFd<'_>>,
    ) -> Self {
        Self {
            stream: stream.get(),
            request: request.get(),
            destination: destination.get(),
            reuse_fd: reuse.map_or(-1, |fd| fd.as_raw_fd()),
            ..Default::default()
        }
    }
}

nix::ioctl_write_ptr!(queue, b'd', 0x05, Queue);

impl<F: AsFd> Client<F> {
    /// Admit output to exact registered storage, optionally after a native reuse fence.
    ///
    /// The kernel validates a supplied sync-file descriptor and retains its fence
    /// on success. It must cover submitted work, not a promise to submit later.
    /// The owner must exclude competing destination access until completion;
    /// registration and readiness snapshots do not reserve storage against other users.
    ///
    /// EAGAIN means no request was admitted. Success consumes its name and one
    /// stream slot, but does not establish valid pixels. No error is retried here,
    /// and closing the client does not acknowledge destination reuse.
    pub fn queue_output(
        &self,
        stream: StreamId,
        request: RequestId,
        destination: DestinationId,
        reuse: Option<BorrowedFd<'_>>,
    ) -> io::Result<()> {
        let input = Queue::new(stream, request, destination, reuse);
        // SAFETY: Input and any supplied fence descriptor remain live through
        // the call. Successful admission retains kernel references, not fd numbers.
        unsafe { queue(self.as_fd().as_raw_fd(), &input) }?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use std::os::fd::AsFd;

    #[test]
    fn queue_layout_matches_the_kernel() {
        assert_eq!(size_of::<Queue>(), 40);
        assert_eq!(offset_of!(Queue, reuse_fd), 24);
        assert_eq!(offset_of!(Queue, reserved), 32);
        assert_eq!(nix::request_code_write!(b'd', 5, 40), 0x4028_6405);
        assert!(RequestId::new(0).is_none());
    }

    #[test]
    fn missing_reuse_is_negative_one_not_standard_input() {
        let file = std::fs::File::open("/dev/null").unwrap();
        for reuse in [None, Some(file.as_fd())] {
            let input = Queue::new(
                StreamId::new(1).unwrap(),
                RequestId::new(2).unwrap(),
                DestinationId::new(3).unwrap(),
                reuse,
            );
            assert_eq!((input.stream, input.request, input.destination), (1, 2, 3));
            assert_eq!(input.reuse_fd, reuse.map_or(-1, |fd| fd.as_raw_fd()));
            assert_eq!((input.flags, input.reserved), (0, 0));
        }
    }
}
