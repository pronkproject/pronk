//! Exported output ownership after compositor-source retirement.

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::OwnedFd;
use std::sync::Arc;

use pronk_gpu::output_pool::{
    AccessReady, FinishedAccess, OutputPool as AccessPool, PendingAccess, Publication,
    PublishPermit, WritePermit, MAX_OUTPUT_BUFFERS,
};
use pronk_gpu::vulkan::{Device, Image, ImageLayout};

use crate::RenderedFrame;

/// Maximum visible and native storage retained by one exported output pool.
pub const MAX_OUTPUT_POOL_BYTES: u64 = 512 * 1024 * 1024;
const OUTPUT_PIXEL_BYTES: u64 = 4;

/// GPU images and access state for one immutable recipient scope.
pub struct OutputPool {
    identity: Arc<()>,
    access: AccessPool,
    images: Vec<Option<Image>>,
    layout: ImageLayout,
}

/// Opaque identity of one output pool and recipient authorization scope.
#[derive(Clone)]
pub struct OutputScope(Arc<()>);

impl PartialEq for OutputScope {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for OutputScope {}

impl OutputPool {
    /// Allocate and prepare every output before source work may use the pool.
    pub async fn new(
        device: &Device,
        width: NonZeroU32,
        height: NonZeroU32,
        modifier: u64,
        capacity: NonZeroUsize,
    ) -> io::Result<Self> {
        validate_request(width, height, capacity)?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(capacity.get())
            .map_err(io::Error::other)?;
        let mut allocation_bytes = 0_u64;
        for _ in 0..capacity.get() {
            let image = device.allocate(width, height, modifier)?;
            allocation_bytes = allocation_bytes
                .checked_add(image.layout().allocation_size)
                .ok_or_else(|| invalid("output pool allocation size overflowed"))?;
            if allocation_bytes > MAX_OUTPUT_POOL_BYTES {
                return Err(invalid("output pool exceeds its native byte limit"));
            }
            images.push(Some(image));
        }
        let layout = images[0]
            .as_ref()
            .expect("nonzero output capacity")
            .layout();
        if images
            .iter()
            .any(|image| image.as_ref().expect("allocated output image").layout() != layout)
        {
            return Err(invalid("output allocator returned inconsistent layouts"));
        }
        let buffers = images
            .iter()
            .map(|image| image.as_ref().expect("allocated output image").export())
            .collect::<io::Result<Vec<OwnedFd>>>()?;
        let mut access = AccessPool::new(buffers)?;
        for slot in 0..images.len() {
            let finished = access.prepare_initial(slot)?.wait().await;
            if !matches!(
                access.complete(finished)?,
                AccessReady::Writable { slot: ready } if ready == slot
            ) {
                return Err(invalid("initial output wait returned the wrong slot"));
            }
        }
        Ok(Self {
            identity: Arc::new(()),
            access,
            images,
            layout,
        })
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn layout(&self) -> ImageLayout {
        self.layout
    }

    pub fn scope(&self) -> OutputScope {
        OutputScope(Arc::clone(&self.identity))
    }

    /// Duplicate one allocation descriptor for transport registration.
    pub fn export(&self, slot: usize) -> io::Result<OwnedFd> {
        self.access.export(slot)
    }

    /// Reserve a destination whose downstream readers have retired.
    pub fn claim(&mut self, slot: usize) -> io::Result<OutputDestination> {
        let image = self
            .images
            .get_mut(slot)
            .ok_or_else(|| invalid("unknown output slot"))?;
        if image.is_none() {
            return Err(invalid("output image is already in use"));
        }
        let permit = self.access.claim(slot)?;
        Ok(OutputDestination {
            pool: Arc::clone(&self.identity),
            slot,
            permit,
            image: image.take().expect("checked output image"),
        })
    }

    /// Enroll a completed GPU write before allowing publication.
    pub fn submit(&mut self, output: CompletedOutput) -> io::Result<PendingOutput> {
        let CompletedOutput {
            pool,
            frame,
            destination,
            permit,
            completion,
            slot,
        } = output;
        if !Arc::ptr_eq(&self.identity, &pool) {
            return Err(invalid("completed output belongs to another pool"));
        }
        let pending = self.access.submitted(permit, completion)?;
        Ok(PendingOutput {
            pool,
            frame,
            destination,
            slot,
            pending,
        })
    }

    /// Apply producer completion and retain the native image for publication.
    pub fn finish(&mut self, output: FinishedOutput) -> io::Result<ReadyOutput> {
        let FinishedOutput {
            pool,
            frame,
            destination,
            slot,
            finished,
        } = output;
        if !Arc::ptr_eq(&self.identity, &pool) {
            return Err(invalid("pending output belongs to another pool"));
        }
        let image = self
            .images
            .get(slot)
            .ok_or_else(|| invalid("completed output has an invalid image slot"))?;
        if image.is_some() {
            return Err(invalid("completed output has an invalid image slot"));
        }
        match self.access.complete(finished)? {
            AccessReady::Publish(permit) if permit.slot() == slot => Ok(ReadyOutput {
                pool,
                frame,
                destination,
                permit,
            }),
            _ => Err(invalid("producer completion returned the wrong slot")),
        }
    }

    /// Transfer one ready output to the registered transport.
    pub fn publish(&mut self, output: ReadyOutput) -> io::Result<(RenderedFrame, PublishedOutput)> {
        let ReadyOutput {
            pool,
            frame,
            destination,
            permit,
        } = output;
        if !Arc::ptr_eq(&self.identity, &pool) {
            return Err(invalid("ready output belongs to another pool"));
        }
        let content_serial = frame.private.content_serial;
        self.access.publish(permit).map(|publication| {
            (
                frame,
                PublishedOutput {
                    pool,
                    image: destination,
                    publication,
                    content_serial,
                },
            )
        })
    }

    /// Begin waiting for readers after the transport returns an output.
    pub fn begin_return(&mut self, output: PublishedOutput) -> io::Result<OutputReturn> {
        let PublishedOutput {
            pool,
            image,
            publication,
            ..
        } = output;
        if !Arc::ptr_eq(&self.identity, &pool) {
            return Err(invalid("published output belongs to another pool"));
        }
        self.access
            .returned(publication)
            .map(|pending| OutputReturn {
                pool,
                image,
                pending,
            })
    }

    /// Make a returned slot writable only after all native readers retire.
    pub fn finish_return(&mut self, returned: CompletedReturn) -> io::Result<usize> {
        let CompletedReturn {
            pool,
            image,
            finished,
        } = returned;
        if !Arc::ptr_eq(&self.identity, &pool) {
            return Err(invalid("completed return belongs to another pool"));
        }
        match self.access.complete(finished)? {
            AccessReady::Writable { slot } => {
                let destination = self
                    .images
                    .get_mut(slot)
                    .ok_or_else(|| invalid("completed return has an invalid image slot"))?;
                if destination.is_some() {
                    return Err(invalid("completed return has an occupied image slot"));
                }
                *destination = Some(image);
                Ok(slot)
            }
            AccessReady::Publish(_) => Err(invalid("reader completion became a publication")),
        }
    }
}

fn validate_request(
    width: NonZeroU32,
    height: NonZeroU32,
    capacity: NonZeroUsize,
) -> io::Result<()> {
    if capacity.get() > MAX_OUTPUT_BUFFERS {
        return Err(invalid("output pool exceeds its supported capacity"));
    }
    let bytes = u64::from(width.get())
        .checked_mul(u64::from(height.get()))
        .and_then(|pixels| pixels.checked_mul(OUTPUT_PIXEL_BYTES))
        .and_then(|bytes| bytes.checked_mul(capacity.get() as u64))
        .ok_or_else(|| invalid("output pool byte size overflowed"))?;
    if bytes > MAX_OUTPUT_POOL_BYTES {
        return Err(invalid("output pool exceeds its logical byte limit"));
    }
    Ok(())
}

/// Exclusive ownership of one writable exported image.
///
/// ```compile_fail
/// use pronk_renderer_worker::{OutputDestination, PrivateBuffer};
/// fn copy_uninitialized(output: OutputDestination, buffer: PrivateBuffer) {
///     output.copy_from(buffer).unwrap();
/// }
/// ```
#[must_use = "copy private pixels into the destination or quarantine its pool"]
pub struct OutputDestination {
    pool: Arc<()>,
    slot: usize,
    permit: WritePermit,
    image: Image,
}

impl OutputDestination {
    /// Copy retired private pixels without retaining a compositor source.
    pub fn copy_from(self, source: RenderedFrame) -> io::Result<CompletedOutput> {
        let Self {
            pool,
            slot,
            permit,
            image,
        } = self;
        let crate::scene_image::CopiedSceneImage {
            scene,
            destination,
            completion,
        } = source.scene.copy_into(image)?;
        Ok(CompletedOutput {
            pool,
            frame: RenderedFrame {
                private: source.private,
                scene,
            },
            destination,
            permit,
            completion,
            slot,
        })
    }
}

/// A completed GPU write that still needs output-pool enrollment.
#[must_use = "enroll the completed write in its output pool"]
pub struct CompletedOutput {
    pool: Arc<()>,
    frame: RenderedFrame,
    destination: Image,
    permit: WritePermit,
    completion: pronk_dmabuf::SyncFile,
    slot: usize,
}

/// An enrolled output awaiting successful producer completion.
///
/// ```compile_fail
/// use pronk_renderer_worker::{OutputPool, PendingOutput};
/// fn publish_before_completion(pool: &mut OutputPool, output: PendingOutput) {
///     pool.publish(output).unwrap();
/// }
/// ```
#[must_use = "finish the producer wait before publishing the output"]
pub struct PendingOutput {
    pool: Arc<()>,
    frame: RenderedFrame,
    destination: Image,
    slot: usize,
    pending: PendingAccess,
}

impl PendingOutput {
    /// Wait without borrowing the pool or preventing other slots from progressing.
    pub async fn wait(self) -> FinishedOutput {
        FinishedOutput {
            pool: self.pool,
            frame: self.frame,
            destination: self.destination,
            slot: self.slot,
            finished: self.pending.wait().await,
        }
    }
}

/// Completed producer wait that only its originating pool may apply.
#[must_use = "apply the completed producer wait to its output pool"]
pub struct FinishedOutput {
    pool: Arc<()>,
    frame: RenderedFrame,
    destination: Image,
    slot: usize,
    finished: FinishedAccess,
}

/// A successful output paired with its independently reusable private buffer.
#[must_use = "publish the output and recover its private buffer"]
pub struct ReadyOutput {
    pool: Arc<()>,
    frame: RenderedFrame,
    destination: Image,
    permit: PublishPermit,
}

/// An output owned by the transport until it reports release.
#[must_use = "retain the publication until transport releases the output"]
pub struct PublishedOutput {
    pool: Arc<()>,
    image: Image,
    publication: Publication,
    content_serial: Option<std::num::NonZeroU64>,
}

impl PublishedOutput {
    pub fn slot(&self) -> usize {
        self.publication.slot()
    }

    pub fn belongs_to(&self, scope: &OutputScope) -> bool {
        Arc::ptr_eq(&self.pool, &scope.0)
    }

    pub fn content_serial(&self) -> Option<std::num::NonZeroU64> {
        self.content_serial
    }
}

/// An output returned by transport and awaiting its native readers.
#[must_use = "wait for native readers before reusing the output"]
pub struct OutputReturn {
    pool: Arc<()>,
    image: Image,
    pending: PendingAccess,
}

impl OutputReturn {
    pub async fn wait(self) -> CompletedReturn {
        CompletedReturn {
            pool: self.pool,
            image: self.image,
            finished: self.pending.wait().await,
        }
    }
}

/// Completed reader wait that only its originating pool may apply.
#[must_use = "apply the completed reader wait to its output pool"]
pub struct CompletedReturn {
    pool: Arc<()>,
    image: Image,
    finished: FinishedAccess,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
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
    fn output_pool_policy_accepts_the_initial_four_k_budget() {
        validate_request(nz32(3840), nz32(2160), nzsize(4)).unwrap();
    }

    #[test]
    fn output_pool_policy_rejects_excess_count_or_storage() {
        assert!(validate_request(nz32(1), nz32(1), nzsize(MAX_OUTPUT_BUFFERS + 1)).is_err());
        assert!(validate_request(nz32(7680), nz32(4320), nzsize(5)).is_err());
    }
}
