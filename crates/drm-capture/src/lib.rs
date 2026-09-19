//! Final-image capture through the experimental anonymous DRM capture interface.
//!
//! Capture authority is separate from modesetting, allocation and renderer access.
//! No operation on this client requires a primary DRM file or exposes source pixels.

mod access;
mod cancel;
mod completion;
mod description;
mod destination;
mod grant;
mod queue;
mod stream;

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

pub use access::Access;
pub use completion::Completion;
pub use description::{Description, OfferId, RequestedLayout};
pub use destination::{Destination, DestinationId, Plane};
pub use grant::{create_grant, Control};
pub use queue::RequestId;
pub use stream::StreamId;

/// One capture descriptor owner, without display-control operations.
///
/// Dropping the client abandons observation. It does not acknowledge completion
/// of destination access and must never be used as permission to recycle buffers.
/// The owner may additionally release a broker session when dropped.
#[derive(Debug)]
pub struct Client<F = OwnedFd> {
    fd: F,
}

impl Client {
    /// Adopt an inherited descriptor after successfully querying its active output.
    ///
    /// Inactive or revoked grants are rejected with the kernel's error. The query
    /// reserves no storage and its offer may change before a stream is opened.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Self::from_owner(fd)
    }
}

impl<F: AsFd> Client<F> {
    /// Retain a descriptor owner after querying its active capture output.
    ///
    /// Failure drops the owner. In particular, a broker session keeps its own
    /// release behavior without the capture client knowing the broker protocol.
    /// Inactive outputs are rejected; acquisition is not a wait for a modeset.
    pub fn from_owner(fd: F) -> io::Result<Self> {
        let client = Self { fd };
        client.describe()?;
        Ok(client)
    }

    /// Return the owner, for example to await explicit broker release.
    ///
    /// No stream is closed and no destination access is acknowledged here.
    pub fn into_owner(self) -> F {
        self.fd
    }
}

impl<F: AsFd> AsFd for Client<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Debug)]
    struct Owner {
        fd: OwnedFd,
        drops: Rc<Cell<usize>>,
    }

    impl AsFd for Owner {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    fn owner() -> (Owner, Rc<Cell<usize>>) {
        let drops = Rc::new(Cell::new(0));
        let owner = Owner {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            drops: drops.clone(),
        };
        (owner, drops)
    }

    #[test]
    fn failed_validation_drops_the_complete_owner() {
        let (owner, drops) = owner();
        let error = Client::from_owner(owner).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ENOTTY));
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn returning_the_owner_does_not_release_it() {
        let (owner, drops) = owner();
        let client = Client { fd: owner };
        let owner = client.into_owner();
        assert_eq!(drops.get(), 0);
        drop(owner);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn dropping_the_client_drops_the_complete_owner() {
        let (owner, drops) = owner();
        let client = Client { fd: owner };
        assert_eq!(drops.get(), 0);
        drop(client);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn operations_borrow_the_descriptor_without_releasing_its_owner() {
        use std::num::NonZeroU32;

        let (owner, drops) = owner();
        let client = Client { fd: owner };
        let stream = StreamId::new(1).unwrap();
        let destination = DestinationId::new(1).unwrap();
        let request = RequestId::new(1).unwrap();
        let one = NonZeroU32::new(1).unwrap();
        let planes = [Plane {
            buffer: client.as_fd(),
            stride: NonZeroU32::new(4).unwrap(),
            offset: 0,
        }];
        let image = Destination {
            width: one,
            height: one,
            format: u32::from_le_bytes(*b"XR24"),
            modifier: 0,
            planes: &planes,
        };
        // No ioctl on /dev/null admits work. Every operation must still use the
        // same owner type, including cleanup after an unsuccessful submission.
        for result in [
            client.describe().map(|_| ()),
            client.register_destination(destination, &image),
            client.queue_output(stream, request, destination, None),
            client.try_dequeue(stream).map(|_| ()),
            client.cancel(stream, request),
            client.unregister_destination(destination),
            client.close_stream(stream),
        ] {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(nix::libc::ENOTTY));
            assert_eq!(drops.get(), 0);
        }
        drop(client);
        assert_eq!(drops.get(), 1);
    }
}
