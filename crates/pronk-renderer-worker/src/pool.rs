//! Bounded private storage reserved independently of scene sources.

use std::io;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use pronk_gpu::vulkan::{Device, PrivateImage};

use crate::SourceAlpha;

/// Maximum number of independently reusable private images in one pool.
pub const MAX_PRIVATE_BUFFERS: usize = 64;
/// Maximum device-memory bytes allocated by one private pool.
pub const MAX_PRIVATE_POOL_BYTES: u64 = 512 * 1024 * 1024;
const PRIVATE_PIXEL_BYTES: u64 = 16;

/// A fixed-size pool of non-exportable rendering buffers.
pub struct PrivatePool {
    identity: Arc<()>,
    device: Device,
    available: Vec<PrivateBuffer>,
    capacity: NonZeroUsize,
    extent: (NonZeroU32, NonZeroU32),
}

impl PrivatePool {
    /// Allocate every buffer before any scene source is claimed.
    pub fn new(
        device: &Device,
        width: NonZeroU32,
        height: NonZeroU32,
        capacity: NonZeroUsize,
    ) -> io::Result<Self> {
        validate_request(width, height, capacity)?;
        let identity = Arc::new(());
        let mut available = Vec::new();
        available
            .try_reserve_exact(capacity.get())
            .map_err(io::Error::other)?;
        let mut allocated_bytes = 0;
        for _ in 0..capacity.get() {
            let image = device.allocate_private(width, height)?;
            allocated_bytes = account_allocation(allocated_bytes, image.allocation_size())?;
            available.push(PrivateBuffer {
                identity: Arc::clone(&identity),
                image,
            });
        }
        Ok(Self {
            identity,
            device: device.clone(),
            available,
            capacity,
            extent: (width, height),
        })
    }

    pub fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }

    pub fn available(&self) -> usize {
        self.available.len()
    }

    pub fn extent(&self) -> (NonZeroU32, NonZeroU32) {
        self.extent
    }

    /// Whether the pool uses the supplied logical Vulkan device instance.
    ///
    /// Ownership remains known while every buffer is checked out. Opening the
    /// same physical GPU again does not produce a compatible private owner.
    pub fn is_owned_by(&self, device: &Device) -> bool {
        self.device.is_same_instance(device)
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

fn account_allocation(total: u64, bytes: u64) -> io::Result<u64> {
    let total = total
        .checked_add(bytes)
        .ok_or_else(|| invalid("private pool allocation size overflowed"))?;
    if total > MAX_PRIVATE_POOL_BYTES {
        return Err(invalid("private pool exceeds its allocation limit"));
    }
    Ok(total)
}

fn validate_request(
    width: NonZeroU32,
    height: NonZeroU32,
    capacity: NonZeroUsize,
) -> io::Result<()> {
    if capacity.get() > MAX_PRIVATE_BUFFERS {
        return Err(invalid("private pool exceeds its buffer limit"));
    }
    let bytes = u64::from(width.get())
        .checked_mul(u64::from(height.get()))
        .and_then(|pixels| pixels.checked_mul(PRIVATE_PIXEL_BYTES))
        .and_then(|bytes| bytes.checked_mul(capacity.get() as u64))
        .ok_or_else(|| invalid("private pool byte size overflowed"))?;
    if bytes > MAX_PRIVATE_POOL_BYTES {
        return Err(invalid("private pool exceeds its byte limit"));
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
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

    pub(super) fn is_owned_by(&self, device: &Device) -> bool {
        self.image.is_owned_by(device)
    }

    /// Initialize private pixels without involving a compositor source.
    pub fn clear_waited(self, rgb: [u8; 3]) -> io::Result<PrivateFrame> {
        let Self { identity, image } = self;
        image.clear_waited(rgb).map(|image| PrivateFrame {
            buffer: Self { identity, image },
            content_serial: None,
            alpha: SourceAlpha::Opaque,
        })
    }
}

/// Initialized private pixels with optional CastKMS content identity.
#[must_use = "copy the frame to output and recover its private buffer"]
pub struct PrivateFrame {
    pub(super) buffer: PrivateBuffer,
    pub(super) content_serial: Option<NonZeroU64>,
    pub(super) alpha: SourceAlpha,
}

impl PrivateFrame {
    pub fn extent(&self) -> (NonZeroU32, NonZeroU32) {
        self.buffer.extent()
    }

    pub fn content_serial(&self) -> Option<NonZeroU64> {
        self.content_serial
    }

    pub fn source_alpha(&self) -> SourceAlpha {
        self.alpha
    }

    pub(super) fn is_owned_by(&self, device: &Device) -> bool {
        self.buffer.is_owned_by(device)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn nz32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nzsize(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[test]
    fn private_pool_policy_accepts_the_initial_four_k_budget() {
        validate_request(nz32(3840), nz32(2160), nzsize(4)).unwrap();
    }

    #[test]
    fn private_pool_policy_rejects_excess_count_or_storage() {
        assert_eq!(
            validate_request(nz32(1), nz32(1), nzsize(MAX_PRIVATE_BUFFERS + 1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_request(nz32(7680), nz32(4320), nzsize(2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn private_pool_policy_rejects_excess_native_allocation() {
        assert_eq!(
            account_allocation(MAX_PRIVATE_POOL_BYTES, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            account_allocation(MAX_PRIVATE_POOL_BYTES - 1, 1).unwrap(),
            MAX_PRIVATE_POOL_BYTES
        );
    }
}
