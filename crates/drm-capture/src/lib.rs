//! Final-image capture through the experimental anonymous DRM capture interface.
//!
//! Capture authority is separate from modesetting, allocation and renderer access.
//! No operation on this client requires a primary DRM file or exposes source pixels.

mod completion;
mod description;
mod destination;
mod grant;
mod queue;
mod stream;

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

pub use completion::Completion;
pub use description::{Description, OfferId};
pub use destination::{Destination, DestinationId, Plane};
pub use grant::{create_grant, Control};
pub use queue::RequestId;
pub use stream::StreamId;

/// One owned capture descriptor, without revocation or display-control authority.
///
/// Dropping the client abandons observation. It does not acknowledge completion
/// of destination access and must never be used as permission to recycle buffers.
#[derive(Debug)]
pub struct Client {
    fd: OwnedFd,
}

impl Client {
    /// Adopt an inherited descriptor after successfully querying its active output.
    ///
    /// Inactive or revoked grants are rejected with the kernel's error. The query
    /// reserves no storage and its offer may change before a stream is opened.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let client = Self { fd };
        client.describe()?;
        Ok(client)
    }
}

impl AsFd for Client {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
