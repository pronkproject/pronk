//! Bounded private storage reserved independently of scene sources.

use std::io;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use pronk_gpu::vulkan::{Device, PrivateImage};

/// A fixed-size pool of non-exportable rendering buffers.
pub struct PrivatePool {
    identity: Arc<()>,
    available: Vec<PrivateBuffer>,
    capacity: NonZeroUsize,
}

impl PrivatePool {
    /// Allocate every buffer before any scene source is claimed.
    pub fn new(
        device: &Device,
        width: NonZeroU32,
        height: NonZeroU32,
        capacity: NonZeroUsize,
    ) -> io::Result<Self> {
        let identity = Arc::new(());
        let mut available = Vec::new();
        available
            .try_reserve_exact(capacity.get())
            .map_err(io::Error::other)?;
        for _ in 0..capacity.get() {
            available.push(PrivateBuffer {
                identity: Arc::clone(&identity),
                image: device.allocate_private(width, height)?,
            });
        }
        Ok(Self {
            identity,
            available,
            capacity,
        })
    }

    pub fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }

    pub fn available(&self) -> usize {
        self.available.len()
    }

    /// Reserve one buffer before attempting to claim a source.
    pub fn take(&mut self) -> Option<PrivateBuffer> {
        self.available.pop()
    }

    /// Return a retired buffer to the pool that allocated it.
    pub fn put(&mut self, buffer: PrivateBuffer) -> Result<(), RejectedBuffer> {
        if !Arc::ptr_eq(&self.identity, &buffer.identity)
            || self.available.len() == self.capacity.get()
        {
            return Err(RejectedBuffer { buffer });
        }
        self.available.push(buffer);
        Ok(())
    }
}

/// One uniquely owned buffer reserved from a [`PrivatePool`].
///
/// No public constructor exists, so safe code cannot wrap arbitrary Vulkan
/// storage or manufacture extra entries for a pool.
#[must_use = "render into the reserved buffer or return it to its private pool"]
pub struct PrivateBuffer {
    pub(super) identity: Arc<()>,
    pub(super) image: PrivateImage,
}

impl PrivateBuffer {
    pub fn extent(&self) -> (NonZeroU32, NonZeroU32) {
        self.image.extent()
    }

    /// Initialize private pixels without involving a compositor source.
    pub fn clear_waited(self, rgb: [u8; 3]) -> io::Result<PrivateFrame> {
        let Self { identity, image } = self;
        image.clear_waited(rgb).map(|image| PrivateFrame {
            buffer: Self { identity, image },
            content_serial: None,
        })
    }
}

/// Initialized private pixels with optional CastKMS content identity.
#[must_use = "copy the frame to output and recover its private buffer"]
pub struct PrivateFrame {
    pub(super) buffer: PrivateBuffer,
    pub(super) content_serial: Option<NonZeroU64>,
}

impl PrivateFrame {
    pub fn extent(&self) -> (NonZeroU32, NonZeroU32) {
        self.buffer.extent()
    }

    pub fn content_serial(&self) -> Option<NonZeroU64> {
        self.content_serial
    }
}

/// A buffer rejected by a pool without losing its unique owner.
pub struct RejectedBuffer {
    buffer: PrivateBuffer,
}

impl RejectedBuffer {
    pub fn into_buffer(self) -> PrivateBuffer {
        self.buffer
    }
}
