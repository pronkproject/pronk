use std::num::NonZeroU32;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

use crate::{Buffer, BufferDescription, Frame};

/// Storage identity for one actor destination, not permission to read its pixels.
///
/// A media transport may duplicate the descriptor when registering its pool.
/// Keep the matching completed `Frame` until the transport returns that use;
/// holding this handle alone does not prevent another capture write. Do not
/// register the storage with a recipient outside its authorization domain.
#[derive(Clone, Debug)]
pub struct BufferHandle(pub(crate) Arc<Buffer>);

impl BufferHandle {
    /// Compare allocation ownership, not a reusable descriptor or request number.
    pub fn contains_frame(&self, frame: &Frame) -> bool {
        Arc::ptr_eq(&self.0, &frame.buffer)
    }

    pub fn stride(&self) -> NonZeroU32 {
        self.0.description.pitch
    }

    pub fn description(&self) -> BufferDescription {
        self.0.description
    }
}

impl AsFd for BufferHandle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
