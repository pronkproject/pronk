//! Blocking native stages, independent of PipeWire publication scheduling.

use anyhow::{ensure, Result};
use drm_display_executor::scheduler::source_use::Submission;
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{CopiedImages, Device, Image, OpaqueLayer, PendingStage};
use std::os::fd::AsFd;

use crate::pattern::{self, Plane};

/// Accepted reads and generator allocations, retained on the blocking worker.
pub struct SubmittedRead {
    originals: Vec<Image>,
    pending: PendingStage,
}

pub fn submit_sources(
    worker: &Device,
    input: Vec<Image>,
    private: Image,
    scene: [Plane; 3],
    permit: Submission<SyncFile>,
) -> Result<SubmittedRead> {
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
    let pending = private.submit_opaque(layers, pattern::BACKGROUND)?;
    let read_done = SyncFile::from_fd(pending.completion().as_fd().try_clone_to_owned()?)?;
    permit.submitted(read_done);
    Ok(SubmittedRead { originals, pending })
}

impl SubmittedRead {
    /// The coordinator resolves source-use accounting before starting this
    /// retirement and output operation. Native reading must finish successfully
    /// before original reuse or copying completed private pixels downstream.
    pub fn copy_output(self, output: Image) -> Result<(Vec<Image>, CopiedImages)> {
        let (private, _) = self.pending.wait()?;
        let input = self
            .originals
            .into_iter()
            .map(|input| input.clear_waited([255; 3]).map(|result| result.0))
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut copied = output.copy_from_waited(private)?;
        copied.source = copied.source.clear_waited([0; 3])?.0;
        Ok((input, copied))
    }
}
