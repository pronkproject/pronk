//! A source-reading copy accepts only an independently available destination.

use std::io;

use drm_display_executor::scene::geometry::SourceRect;
use drm_display_executor::scene::transform::Transform;
use pronk_dmabuf::SyncFile;

use super::{Image, SourceImage};

mod geometry;
mod pending;
mod submit;
use geometry::{Copy, Transfer};
pub use pending::PendingStage;
use submit::submit;

impl SourceImage {
    /// Read this use into independently available private staging storage.
    ///
    /// The caller supplies an exclusive private destination, with no downstream
    /// submissions racing its native dependency snapshot. Pending destination
    /// reuse returns `WouldBlock` before waiting for or reading the source.
    /// Source-producer waits are native dependencies, not downstream reuse waits.
    ///
    /// Run on a blocking graphics worker. The import is consumed and destroyed
    /// after native reading ends; successful return supplies the initialized
    /// staging image and actual completion covering only this source-to-stage
    /// operation. Source authority and protocol release remain caller duties.
    pub fn copy_into_waited(self, destination: Image) -> io::Result<(Image, SyncFile)> {
        let copy = Copy::whole(self.layout(), destination.layout())?;
        self.copy_waited(destination, copy)
    }

    /// Copy a visible integral crop over a cleared private staging background.
    ///
    /// Placement is unscaled and unrotated. Copied bytes, including pixel alpha,
    /// are preserved: this is not alpha blending or color conversion. An opaque
    /// source in the same encoded RGB domain is required to use this operation
    /// as an opaque plane composition. The background has opaque alpha.
    ///
    /// A mismatched crop or fully offscreen placement is rejected before source
    /// waits or native submission. A caller with no visible source should clear
    /// private storage without acquiring a source use. All source authority,
    /// destination availability and blocking-worker rules of `copy_into_waited`
    /// apply unchanged. Successful return initializes the complete destination.
    pub fn copy_region_into_waited(
        self,
        destination: Image,
        source: SourceRect,
        placement: [i32; 2],
        background: [u8; 3],
    ) -> io::Result<(Image, SyncFile)> {
        let copy = Copy::placed(
            self.layout(),
            destination.layout(),
            source,
            placement,
            background,
        )?;
        self.copy_waited(destination, copy)
    }

    fn copy_waited(self, destination: Image, copy: Copy) -> io::Result<(Image, SyncFile)> {
        submit(
            destination,
            vec![(self, Transfer::Copy(copy.region))],
            copy.background,
        )?
        .wait()
    }
}

/// An owned source use and a requested opaque, integral plane placement.
///
/// Pixels must have opaque alpha and share the destination's encoded RGB
/// domain. This is a copy profile, not alpha blending or color conversion.
pub struct OpaqueLayer {
    source: SourceImage,
    crop: SourceRect,
    placement: [i32; 2],
    transform: Transform,
}

impl OpaqueLayer {
    /// Retain a source and requested crop; composition validates their match
    /// and visibility against the actual destination before any producer wait.
    pub fn new(source: SourceImage, crop: SourceRect, placement: [i32; 2]) -> Self {
        Self {
            source,
            crop,
            placement,
            transform: Transform::default(),
        }
    }

    /// Select reflection or a half turn. Quarter turns are rejected by
    /// composition before producer waits; the source crop is never scaled.
    pub fn with_transform(mut self, transform: Transform) -> Self {
        self.transform = transform;
        self
    }
}

impl Image {
    /// Compose distinct imported allocations in bottom-to-top order.
    ///
    /// Private destination availability is checked before any producer wait.
    /// Every source import is retained through native completion and destroyed
    /// before successful return. One completion covers all source reads and the
    /// private write, without including later output or encoder dependencies.
    ///
    /// The caller excludes external destination submissions and source reuse,
    /// and retains source authority until completion. Run on a blocking worker.
    /// Aliased allocations, mismatched devices, invalid crops and fully invisible
    /// layers are rejected before producer waits. Omit invisible source uses;
    /// an empty list initializes the background without reading any source.
    pub fn compose_opaque_waited(
        self,
        layers: Vec<OpaqueLayer>,
        background: [u8; 3],
    ) -> io::Result<(Image, SyncFile)> {
        self.submit_opaque(layers, background)?.wait()
    }

    /// Submit private composition and return its native completion record.
    ///
    /// Validation and producer waits use the same blocking-worker contract as
    /// [`Self::compose_opaque_waited`]. Return does not wait for the submitted
    /// composition to finish. The returned owner retains all source imports
    /// and private storage, exposing pixels only after successful native wait.
    /// Its record covers accepted source reads, not later output operations.
    pub fn submit_opaque(
        self,
        layers: Vec<OpaqueLayer>,
        background: [u8; 3],
    ) -> io::Result<PendingStage> {
        let sources = layers
            .into_iter()
            .map(|layer| {
                let transfer = Transfer::placed(
                    layer.source.layout(),
                    self.layout(),
                    layer.crop,
                    layer.placement,
                    layer.transform,
                )?;
                Ok((layer.source, transfer))
            })
            .collect::<io::Result<Vec<_>>>()?;
        submit(self, sources, Some(background))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod region_tests;

#[cfg(test)]
mod layered_tests;
