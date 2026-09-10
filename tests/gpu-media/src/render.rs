//! Blocking native stages, independent of PipeWire publication scheduling.

use anyhow::{ensure, Result};
use drm_display_executor::scene::{blend::Blend, transform::Transform};
use drm_display_executor::scheduler::source_use::Submission;
use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{Blender, Device, Gamma, Image, PendingPrivateRead, PrivateImage};
use std::num::NonZeroU32;
use std::os::fd::AsFd;
use std::time::Instant;

mod timing;
pub use timing::{Report, Timings};

use crate::pattern::{self, Plane};

/// Accepted reads and generator allocations, retained on the blocking worker.
pub struct SubmittedRead {
    originals: Vec<Image>,
    pending: Vec<(PendingPrivateRead, Plane)>,
    output: PrivateImage,
    blender: Blender,
    gamma: Gamma,
    timing: Timings,
}

/// Independently available allocations, prepared before acquiring source uses.
pub struct PrivateStorage {
    inputs: Vec<PrivateImage>,
    output: PrivateImage,
    blender: Blender,
    gamma: Gamma,
}

impl PrivateStorage {
    pub fn allocate(worker: &Device) -> Result<Self> {
        let nz = |value| NonZeroU32::new(value).unwrap();
        let inputs = pattern::scene(0)
            .into_iter()
            .map(|plane| {
                worker.allocate_private(
                    nz(plane.crop.image().width()),
                    nz(plane.crop.image().height()),
                )
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let output = worker.allocate_private(nz(pattern::WIDTH), nz(pattern::HEIGHT))?;
        let blender = worker.create_blender()?;
        let gamma = worker.create_gamma(&pattern::GAMMA)?;
        Ok(Self {
            inputs,
            output,
            blender,
            gamma,
        })
    }
}

pub struct Rendered {
    pub originals: Vec<Image>,
    pub private: PrivateStorage,
    pub output: Image,
    pub completion: SyncFile,
    pub timing: Timings,
}

pub fn submit_sources(
    worker: &Device,
    input: Vec<Image>,
    private: PrivateStorage,
    scene: [Plane; 3],
    permits: Vec<Submission<Option<SyncFile>>>,
) -> Result<SubmittedRead> {
    let started = Instant::now();
    ensure!(
        input.len() == scene.len()
            && private.inputs.len() == scene.len()
            && permits.len() == scene.len(),
        "source allocation count differs from scene"
    );
    let mut originals = Vec::with_capacity(scene.len());
    let mut pending = Vec::with_capacity(scene.len());
    for (((input, plane), destination), permit) in input
        .into_iter()
        .zip(scene)
        .zip(private.inputs)
        .zip(permits)
    {
        let (input, producer) = input.clear_waited(plane.color)?;
        // SAFETY: Matching native physical-device/driver identities, exact
        // allocator metadata and identical image profile. Clear completed
        // foreign GENERAL release; no source writer runs before read completion.
        let source = unsafe { worker.import_source(input.export()?, input.layout(), producer) }?;
        let read = source.submit_private_copy(destination)?;
        let record = read
            .completion()
            .map(|sync| SyncFile::from_fd(sync.as_fd().try_clone_to_owned()?))
            .transpose()?;
        permit.submitted(record);
        pending.push((read, plane));
        originals.push(input);
    }
    Ok(SubmittedRead {
        originals,
        pending,
        output: private.output,
        blender: private.blender,
        gamma: private.gamma,
        timing: Timings {
            submission: started.elapsed(),
            ..Timings::default()
        },
    })
}

impl SubmittedRead {
    /// The coordinator resolves source-use accounting before starting this
    /// retirement and output operation. Native reading must finish successfully
    /// before original reuse or copying completed private pixels downstream.
    pub fn copy_output(
        self,
        worker: &Device,
        output_worker: &Device,
        output: Image,
    ) -> Result<Rendered> {
        ensure!(
            worker.identity() == output_worker.identity(),
            "source and output devices differ"
        );
        let mut timing = self.timing;
        let started = Instant::now();
        let layers = self
            .pending
            .into_iter()
            .map(|(pending, plane)| pending.wait().map(|image| (image, plane)))
            .collect::<std::io::Result<Vec<_>>>()?;
        timing.retirement = started.elapsed();
        let started = Instant::now();
        let input = self
            .originals
            .into_iter()
            .map(|input| input.clear_waited([255; 3]).map(|result| result.0))
            .collect::<std::io::Result<Vec<_>>>()?;
        timing.overwrites = started.elapsed();
        let started = Instant::now();
        let mut private = self.output.clear_waited(pattern::BACKGROUND)?;
        let mut inputs = Vec::with_capacity(layers.len());
        for (source, plane) in layers {
            let result = self.blender.blend_region_waited(
                private,
                source,
                plane.crop,
                plane.placement,
                Transform::default(),
                Blend::default(),
            )?;
            inputs.push(result.source);
            private = result.destination;
        }
        timing.composition = started.elapsed();
        let started = Instant::now();
        private = self.gamma.apply_waited(private)?;
        timing.color = started.elapsed();
        let started = Instant::now();
        // Only an internal bridge crosses Vulkan devices. The exported capture
        // destination is allocated and accessed exclusively by the output side.
        let layout = output.layout();
        let bridge = worker.allocate(layout.width, layout.height, layout.modifier)?;
        let copied = private.copy_into_waited(bridge)?;
        let bridge_fd = copied.destination.export()?;
        let bridge_layout = copied.destination.layout();
        drop(copied.destination);
        // SAFETY: Exact allocator metadata and matching physical/driver identity.
        // The bridge has completed foreign release and is not shared with any
        // consumer. Its source-side Vulkan image and memory owners are gone.
        let bridge =
            unsafe { output_worker.import_source(bridge_fd, bridge_layout, copied.completion) }?;
        let (output, completion) = bridge.copy_into_waited(output)?;
        timing.output = started.elapsed();
        let started = Instant::now();
        let private = PrivateStorage {
            inputs,
            output: copied.source.clear_waited([0; 3])?,
            blender: self.blender,
            gamma: self.gamma,
        };
        timing.overwrites += started.elapsed();
        Ok(Rendered {
            originals: input,
            private,
            output,
            completion,
            timing,
        })
    }
}
