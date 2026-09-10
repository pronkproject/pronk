//! Blocking native stages, independent of PipeWire publication scheduling.

use anyhow::{ensure, Result};
use drm_display_executor::scheduler::source_use::Submission;
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{CopiedImages, Device, Image, OpaqueLayer};

use crate::pattern::{self, Plane};

/// Completed private pixels and the generator's own reusable source allocations.
pub struct ReadStage {
    originals: Vec<Image>,
    private: Image,
}

pub fn read_sources(
    worker: &Device,
    input: Vec<Image>,
    private: Image,
    scene: [Plane; 3],
    permit: Submission<SyncFile>,
) -> Result<ReadStage> {
    ensure!(
        input.len() == scene.len(),
        "source allocation count differs from scene"
    );
    let mut originals = Vec::with_capacity(scene.len());
    let mut layers = Vec::with_capacity(scene.len());
    for (input, plane) in input.into_iter().zip(scene) {
        let (input, producer) = input.clear_waited(plane.color)?;
        // SAFETY: Matching native physical-device/driver identities, exact
        // allocator metadata and identical image profile. Clear completed
        // foreign GENERAL release; no source writer runs before read completion.
        let source = unsafe { worker.import_source(input.export()?, input.layout(), producer) }?;
        layers.push(OpaqueLayer::new(source, plane.crop, plane.placement));
        originals.push(input);
    }
    let (private, read_done) = private.compose_opaque_waited(layers, pattern::BACKGROUND)?;
    permit.submitted(read_done);
    Ok(ReadStage { originals, private })
}

impl ReadStage {
    /// The coordinator resolves source-use accounting before starting this
    /// independent output operation. Only generator-owned allocations remain.
    pub fn copy_output(self, output: Image) -> Result<(Vec<Image>, CopiedImages)> {
        let input = self
            .originals
            .into_iter()
            .map(|input| input.clear_waited([255; 3]).map(|result| result.0))
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut copied = output.copy_from_waited(self.private)?;
        copied.source = copied.source.clear_waited([0; 3])?.0;
        Ok((input, copied))
    }
}
