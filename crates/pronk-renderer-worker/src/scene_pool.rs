//! Profile-bound private storage for several complete scenes.

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;

use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;

use crate::pool::{
    SceneBinding, SceneBufferRole, MAX_PRIVATE_BUFFERS, MAX_PRIVATE_POOL_BYTES, PRIVATE_PIXEL_BYTES,
};
use crate::{PrivateBuffer, PrivatePool, RejectedBuffer};

/// Maximum source layers retained by one prepared scene profile.
pub const MAX_SCENE_LAYERS: usize = 24;

/// Source-sized intermediates and final images for one qualified scene profile.
///
/// Every layer has a distinct provenance domain even when dimensions match.
/// The final-image pool may remain checked out for output while completed
/// source intermediates return independently. Their capacities are separate;
/// one complete reservation still requires both kinds to be available.
pub struct ScenePool {
    profile: Arc<()>,
    destination: PrivatePool,
    sources: Vec<PrivatePool>,
    source_capacity: NonZeroUsize,
}

impl ScenePool {
    /// Allocate complete scene slots under one aggregate native-memory budget.
    pub(crate) fn new(
        device: &Device,
        output: Extent,
        source_extents: impl ExactSizeIterator<Item = Extent> + Clone,
        final_capacity: NonZeroUsize,
        source_capacity: NonZeroUsize,
        profile: &Arc<()>,
    ) -> io::Result<Self> {
        validate_request(
            output,
            source_extents.clone(),
            final_capacity,
            source_capacity,
        )?;
        let mut allocated_bytes = 0;
        let destination = PrivatePool::new_accounted(
            device,
            nonzero(output.width()),
            nonzero(output.height()),
            final_capacity,
            &mut allocated_bytes,
            Some(SceneBinding::new(profile, SceneBufferRole::Destination)),
        )?;
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(source_extents.len())
            .map_err(io::Error::other)?;
        for (index, extent) in source_extents.enumerate() {
            sources.push(PrivatePool::new_accounted(
                device,
                nonzero(extent.width()),
                nonzero(extent.height()),
                source_capacity,
                &mut allocated_bytes,
                Some(SceneBinding::new(profile, SceneBufferRole::Source(index))),
            )?);
        }
        Ok(Self {
            profile: Arc::clone(profile),
            destination,
            sources,
            source_capacity,
        })
    }

    pub fn final_capacity(&self) -> NonZeroUsize {
        self.destination.capacity()
    }

    pub fn source_capacity(&self) -> NonZeroUsize {
        self.source_capacity
    }

    pub fn available(&self) -> usize {
        self.sources
            .iter()
            .fold(self.destination.available(), |available, source| {
                available.min(source.available())
            })
    }

    pub(crate) fn belongs_to(&self, profile: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.profile, profile)
    }

    /// Reserve one destination and every ordered source without partial checkout.
    pub fn take(&mut self) -> io::Result<Option<SceneBuffers>> {
        if self.available() == 0 {
            return Ok(None);
        }
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(self.sources.len())
            .map_err(io::Error::other)?;
        let destination = self
            .destination
            .take()
            .expect("availability covers the scene destination");
        sources.extend(self.sources.iter_mut().map(|source| {
            source
                .take()
                .expect("availability covers every scene source")
        }));
        Ok(Some(SceneBuffers {
            destination,
            sources,
        }))
    }

    /// Return an unused complete reservation without changing any partial state.
    pub fn restore(&mut self, buffers: SceneBuffers) -> Result<(), RejectedSceneBuffers> {
        if !self.destination.accepts(&buffers.destination)
            || !self.accepts_sources(&buffers.sources)
        {
            return Err(RejectedSceneBuffers { buffers });
        }
        self.destination.put_validated(buffers.destination);
        for (pool, source) in self.sources.iter_mut().zip(buffers.sources) {
            pool.put_validated(source);
        }
        Ok(())
    }

    /// Return completed source intermediates in profile order.
    pub fn restore_sources(
        &mut self,
        sources: Vec<PrivateBuffer>,
    ) -> Result<(), RejectedSceneSources> {
        if !self.accepts_sources(&sources) {
            return Err(RejectedSceneSources { sources });
        }
        for (pool, source) in self.sources.iter_mut().zip(sources) {
            pool.put_validated(source);
        }
        Ok(())
    }

    /// Return a final private image after its output copy retires.
    pub fn restore_destination(
        &mut self,
        destination: PrivateBuffer,
    ) -> Result<(), RejectedBuffer> {
        self.destination.put(destination)
    }

    fn accepts_sources(&self, sources: &[PrivateBuffer]) -> bool {
        sources.len() == self.sources.len()
            && self
                .sources
                .iter()
                .zip(sources)
                .all(|(pool, source)| pool.accepts(source))
    }
}

/// One all-or-none reservation in profile order from a [`ScenePool`].
pub struct SceneBuffers {
    pub destination: PrivateBuffer,
    pub sources: Vec<PrivateBuffer>,
}

/// A complete reservation rejected without returning any of its buffers.
pub struct RejectedSceneBuffers {
    buffers: SceneBuffers,
}

impl RejectedSceneBuffers {
    pub fn into_buffers(self) -> SceneBuffers {
        self.buffers
    }
}

/// Ordered source buffers rejected without partially changing their pools.
pub struct RejectedSceneSources {
    sources: Vec<PrivateBuffer>,
}

impl RejectedSceneSources {
    pub fn into_sources(self) -> Vec<PrivateBuffer> {
        self.sources
    }
}

fn validate_request(
    output: Extent,
    mut source_extents: impl ExactSizeIterator<Item = Extent>,
    final_capacity: NonZeroUsize,
    source_capacity: NonZeroUsize,
) -> io::Result<()> {
    if final_capacity.get() > MAX_PRIVATE_BUFFERS || source_capacity.get() > MAX_PRIVATE_BUFFERS {
        return Err(invalid("scene pool exceeds its buffer-count limit"));
    }
    if source_extents.len() > MAX_SCENE_LAYERS {
        return Err(invalid("scene exceeds its private layer limit"));
    }
    let output_pixels = u64::from(output.width())
        .checked_mul(u64::from(output.height()))
        .and_then(|pixels| pixels.checked_mul(final_capacity.get() as u64));
    let source_pixels = source_extents.try_fold(0_u64, |pixels, extent| {
        u64::from(extent.width())
            .checked_mul(u64::from(extent.height()))
            .and_then(|layer| layer.checked_mul(source_capacity.get() as u64))
            .and_then(|layer| pixels.checked_add(layer))
    });
    let bytes = output_pixels
        .and_then(|output| source_pixels.and_then(|sources| output.checked_add(sources)))
        .and_then(|pixels| pixels.checked_mul(PRIVATE_PIXEL_BYTES))
        .ok_or_else(|| invalid("scene private storage size overflowed"))?;
    if bytes > MAX_PRIVATE_POOL_BYTES {
        return Err(invalid("scene private storage exceeds its byte limit"));
    }
    Ok(())
}

fn nonzero(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("scene extents are nonzero")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(width: u32, height: u32) -> Extent {
        Extent::new(width, height).unwrap()
    }

    #[test]
    fn aggregate_policy_counts_each_capacity_and_source() {
        let output = extent(3840, 2160);
        let sources = [output];
        assert!(validate_request(
            output,
            sources.into_iter(),
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
        )
        .is_ok());
        assert!(validate_request(
            output,
            sources.into_iter(),
            NonZeroUsize::new(3).unwrap(),
            NonZeroUsize::new(1).unwrap(),
        )
        .is_ok());
        assert_eq!(
            validate_request(
                output,
                sources.into_iter(),
                NonZeroUsize::new(3).unwrap(),
                NonZeroUsize::new(2).unwrap(),
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn aggregate_policy_bounds_layer_count() {
        let output = extent(1, 1);
        let sources = vec![output; MAX_SCENE_LAYERS + 1];
        assert_eq!(
            validate_request(
                output,
                sources.into_iter(),
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(1).unwrap(),
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn background_only_scene_still_bounds_both_capacities() {
        let output = extent(1, 1);
        assert_eq!(
            validate_request(
                output,
                std::iter::empty(),
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(MAX_PRIVATE_BUFFERS + 1).unwrap(),
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU selection"]
    fn reservations_and_returns_preserve_each_role() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
        let device = Device::open(node).unwrap();
        let output = extent(8, 6);
        let sources = [extent(4, 3), extent(2, 5)];
        let mut pool = ScenePool::new(
            &device,
            output,
            sources.into_iter(),
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(2).unwrap(),
            &Arc::new(()),
        )
        .unwrap();
        assert_eq!(pool.final_capacity().get(), 2);
        assert_eq!(pool.source_capacity().get(), 2);
        let first = pool.take().unwrap().unwrap();
        let second = pool.take().unwrap().unwrap();
        assert!(pool.take().unwrap().is_none());
        assert_eq!(first.destination.extent(), (nonzero(8), nonzero(6)));
        assert_eq!(first.sources[0].extent(), (nonzero(4), nonzero(3)));
        assert_eq!(first.sources[1].extent(), (nonzero(2), nonzero(5)));
        assert!(pool.restore(first).is_ok());
        assert_eq!(pool.available(), 1);
        assert!(pool.restore(second).is_ok());
        assert_eq!(pool.available(), 2);
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU selection"]
    fn source_storage_cycles_while_final_images_accumulate() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
        let device = Device::open(node).unwrap();
        let output = extent(8, 6);
        let sources = [extent(4, 3), extent(2, 5)];
        let mut pool = ScenePool::new(
            &device,
            output,
            sources.into_iter(),
            NonZeroUsize::new(3).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &Arc::new(()),
        )
        .unwrap();
        assert_eq!(pool.final_capacity().get(), 3);
        assert_eq!(pool.source_capacity().get(), 1);

        let mut held_destinations = Vec::new();
        for _ in 0..3 {
            let reservation = pool.take().unwrap().unwrap();
            assert!(pool.take().unwrap().is_none());
            held_destinations.push(reservation.destination);
            assert!(pool.restore_sources(reservation.sources).is_ok());
        }
        assert!(pool.take().unwrap().is_none());
        for destination in held_destinations {
            assert!(pool.restore_destination(destination).is_ok());
        }
        assert_eq!(pool.available(), 1);
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU selection"]
    fn invalid_source_order_is_rejected_atomically() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
        let device = Device::open(node).unwrap();
        let output = extent(8, 6);
        let sources = [extent(4, 3), extent(2, 5)];
        let mut pool = ScenePool::new(
            &device,
            output,
            sources.into_iter(),
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &Arc::new(()),
        )
        .unwrap();
        let mut buffers = pool.take().unwrap().unwrap();
        buffers.sources.swap(0, 1);
        let error = pool.restore(buffers).err().unwrap();
        assert_eq!(pool.available(), 0);
        let mut buffers = error.into_buffers();
        buffers.sources.swap(0, 1);
        assert!(pool.restore(buffers).is_ok());
        assert_eq!(pool.available(), 1);
    }
}
