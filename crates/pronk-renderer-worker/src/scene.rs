//! Complete private-scene execution after compositor sources retire.

use std::io;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    geometry::{DestinationRect, Extent, SourceRect},
    transform::Transform,
};
use pronk_gpu::vulkan::{
    Blender, ColorPipelineProgram, Device, OutputColorProgram, PrivateLayer, SceneRequirements,
    SourceRequirements,
};

use crate::pool::SceneBufferRole;
use crate::scene_pool::{ScenePool, MAX_SCENE_LAYERS};
use crate::{PrivateBuffer, PrivateFrame, SourceAlpha};

struct LayerPlan {
    source: SourceRequirements,
    crop: SourceRect,
    destination: DestinationRect,
    transform: Transform,
    blend: Blend,
    color: ColorPipelineProgram,
}

/// Prepared execution state for one qualified whole-scene profile.
pub struct SceneComposer {
    storage: SceneStorageProfile,
    blender: Blender,
    color: OutputColorProgram,
    layers: Vec<LayerPlan>,
}

/// Reusable private storage identity for compatible complete scenes.
#[derive(Clone)]
pub struct SceneStorageProfile {
    profile: Arc<()>,
    device: Device,
    output: Extent,
    sources: Vec<SourceRequirements>,
}

impl SceneStorageProfile {
    /// Define the ordered private storage roles for one scene layout.
    pub fn new(
        device: &Device,
        output: Extent,
        sources: &[SourceRequirements],
    ) -> io::Result<Self> {
        if sources.len() > MAX_SCENE_LAYERS {
            return Err(invalid("scene exceeds its private layer limit"));
        }
        device.check_private_image(nonzero(output.width()), nonzero(output.height()))?;
        for source in sources {
            let width = nonzero(source.extent.width());
            let height = nonzero(source.extent.height());
            device.check_source_image(source.format, width, height, source.modifier)?;
            device.check_private_image(width, height)?;
        }
        let mut owned_sources = Vec::new();
        owned_sources
            .try_reserve_exact(sources.len())
            .map_err(io::Error::other)?;
        owned_sources.extend_from_slice(sources);
        Ok(Self {
            profile: Arc::new(()),
            device: device.clone(),
            output,
            sources: owned_sources,
        })
    }

    pub fn output(&self) -> Extent {
        self.output
    }

    pub fn layer_count(&self) -> usize {
        self.sources.len()
    }

    pub fn source_requirements(&self, index: usize) -> Option<SourceRequirements> {
        self.sources.get(index).copied()
    }

    pub(crate) fn profile(&self) -> &Arc<()> {
        &self.profile
    }

    /// Allocate bounded final and source storage reusable across scene jobs.
    pub fn create_pool(
        &self,
        final_capacity: NonZeroUsize,
        source_capacity: NonZeroUsize,
    ) -> io::Result<ScenePool> {
        ScenePool::new(
            &self.device,
            self.output,
            self.sources.iter().map(|source| source.extent),
            final_capacity,
            source_capacity,
            &self.profile,
        )
    }

    fn accepts(&self, scene: SceneRequirements<'_>) -> bool {
        self.output == scene.output
            && self.sources.len() == scene.layers.len()
            && self
                .sources
                .iter()
                .zip(scene.layers)
                .all(|(source, layer)| *source == layer.source)
    }
}

impl SceneComposer {
    /// Qualify and prepare one complete scene before source acquisition.
    pub fn new(device: &Device, scene: SceneRequirements<'_>) -> io::Result<Self> {
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(scene.layers.len())
            .map_err(io::Error::other)?;
        sources.extend(scene.layers.iter().map(|layer| layer.source));
        let storage = SceneStorageProfile::new(device, scene.output, &sources)?;
        Self::with_storage(&storage, scene)
    }

    /// Qualify scene operations against an existing private storage layout.
    pub fn with_storage(
        storage: &SceneStorageProfile,
        scene: SceneRequirements<'_>,
    ) -> io::Result<Self> {
        if !storage.accepts(scene) {
            return Err(invalid("scene does not match its private storage layout"));
        }
        let device = &storage.device;
        let blender = device.create_blender()?;
        blender.check_scene(scene)?;
        let color = device.create_output_color(scene.output, scene.color)?;
        let mut layers = Vec::new();
        layers
            .try_reserve_exact(scene.layers.len())
            .map_err(io::Error::other)?;
        for layer in scene.layers {
            layers.push(LayerPlan {
                source: layer.source,
                crop: layer.crop,
                destination: layer.destination,
                transform: layer.transform,
                blend: layer.blend,
                color: device.create_color_pipeline(layer.source.extent, layer.color)?,
            });
        }
        Ok(Self {
            storage: storage.clone(),
            blender,
            color,
            layers,
        })
    }

    pub fn output(&self) -> Extent {
        self.storage.output
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Return the exact external image profile qualified for one layer.
    pub fn source_requirements(&self, index: usize) -> Option<SourceRequirements> {
        self.layers.get(index).map(|layer| layer.source)
    }

    pub(crate) fn accepts_source_stage(
        &self,
        index: usize,
        source: &pronk_gpu::vulkan::SourceImage,
        destination: &PrivateBuffer,
    ) -> bool {
        let Some(plan) = self.layers.get(index) else {
            return false;
        };
        let layout = source.layout();
        plan.source
            == SourceRequirements {
                format: layout.format,
                extent: Extent::new(layout.width.get(), layout.height.get())
                    .expect("source image extents are nonzero"),
                modifier: layout.modifier,
            }
            && source.is_owned_by(&self.storage.device)
            && destination.is_owned_by(&self.storage.device)
            && destination.matches_scene(&self.storage.profile, SceneBufferRole::Source(index))
    }

    pub(crate) fn profile(&self) -> &Arc<()> {
        &self.storage.profile
    }

    pub(crate) fn device(&self) -> &Device {
        &self.storage.device
    }

    pub fn storage(&self) -> &SceneStorageProfile {
        &self.storage
    }

    /// Allocate independently bounded final and source storage for this profile.
    ///
    /// A final image may remain checked out while a smaller set of source
    /// intermediates cycles through later compositions.
    pub fn create_pool(
        &self,
        final_capacity: NonZeroUsize,
        source_capacity: NonZeroUsize,
    ) -> io::Result<ScenePool> {
        self.storage.create_pool(final_capacity, source_capacity)
    }

    /// Compose qualified source frames and apply the complete output color path.
    ///
    /// Sources must be supplied in the profile's bottom-to-top order. Their
    /// dimensions match the source storage in that profile. Compositor-source
    /// access has already ended; only independent private allocations enter
    /// this operation. Predictable input mismatches return every buffer
    /// untouched. A native failure consumes all affected buffers because their
    /// pixel validity may be unknown.
    pub fn compose_and_wait(
        &self,
        inputs: SceneInputs,
    ) -> Result<ComposedFrame, SceneCompositionError> {
        if let Err(cause) = self.check_inputs(&inputs) {
            return Err(SceneCompositionError::Rejected(RejectedScene {
                inputs,
                cause,
            }));
        }
        let mut source_identities = Vec::new();
        let mut layers = Vec::new();
        let mut returned_sources = Vec::new();
        let capacity = inputs.frames.layers.len();
        if let Err(cause) = reserve(&mut source_identities, capacity)
            .and_then(|()| reserve(&mut layers, capacity))
            .and_then(|()| reserve(&mut returned_sources, capacity))
        {
            return Err(SceneCompositionError::Rejected(RejectedScene {
                inputs,
                cause,
            }));
        }
        let SceneInputs {
            destination,
            frames:
                SceneFrames {
                    layers: sources,
                    content_serial,
                },
            background,
        } = inputs;
        let PrivateBuffer {
            identity: destination_identity,
            image: destination,
        } = destination;
        for (frame, plan) in sources.into_iter().zip(&self.layers) {
            let PrivateFrame {
                buffer: PrivateBuffer { identity, image },
                alpha,
                ..
            } = frame;
            let image = plan
                .color
                .apply_and_wait(image)
                .map_err(SceneCompositionError::Native)?;
            source_identities.push(identity);
            layers.push(PrivateLayer::new(
                image,
                plan.crop,
                plan.destination,
                plan.transform,
                effective_blend(alpha, plan.blend),
            ));
        }
        let composed = self
            .blender
            .compose_and_wait(destination, background, layers)
            .map_err(SceneCompositionError::Native)?;
        let destination = self
            .color
            .apply_and_wait(composed.destination)
            .map_err(SceneCompositionError::Native)?;
        for (identity, image) in source_identities.into_iter().zip(composed.sources) {
            returned_sources.push(PrivateBuffer { identity, image });
        }
        Ok(ComposedFrame {
            sources: returned_sources,
            frame: PrivateFrame {
                buffer: PrivateBuffer {
                    identity: destination_identity,
                    image: destination,
                },
                content_serial: Some(content_serial),
                alpha: SourceAlpha::Opaque,
            },
        })
    }

    fn check_inputs(&self, inputs: &SceneInputs) -> io::Result<()> {
        if inputs.destination.extent()
            != (
                nonzero(self.storage.output.width()),
                nonzero(self.storage.output.height()),
            )
            || !inputs.destination.is_owned_by(&self.storage.device)
            || !inputs
                .destination
                .matches_scene(&self.storage.profile, SceneBufferRole::Destination)
        {
            return Err(invalid(
                "scene destination does not match its qualified output",
            ));
        }
        if inputs.frames.layers.len() != self.layers.len() {
            return Err(invalid(
                "scene source count does not match its qualified layers",
            ));
        }
        for (index, (source, plan)) in inputs.frames.layers.iter().zip(&self.layers).enumerate() {
            if source.extent()
                != (
                    nonzero(plan.source.extent.width()),
                    nonzero(plan.source.extent.height()),
                )
                || !source.is_owned_by(&self.storage.device)
                || !source
                    .buffer
                    .matches_scene(&self.storage.profile, SceneBufferRole::Source(index))
            {
                return Err(invalid("scene source does not match its qualified layer"));
            }
        }
        Ok(())
    }
}

fn effective_blend(alpha: SourceAlpha, blend: Blend) -> Blend {
    match alpha {
        SourceAlpha::Opaque => Blend {
            pixel: PixelBlend::None,
            ..blend
        },
        SourceAlpha::Channel => blend,
    }
}

fn nonzero(value: u32) -> std::num::NonZeroU32 {
    std::num::NonZeroU32::new(value).expect("scene extents are nonzero")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn reserve<T>(storage: &mut Vec<T>, capacity: usize) -> io::Result<()> {
    storage
        .try_reserve_exact(capacity)
        .map_err(io::Error::other)
}

/// Private layer frames carrying one complete scene content identity.
///
/// Construction verifies that every layer carries the complete scene's content
/// identity. This prevents independently completed old and new frames from
/// being assembled into a torn scene.
pub struct SceneFrames {
    layers: Vec<PrivateFrame>,
    content_serial: NonZeroU64,
}

impl SceneFrames {
    pub fn new(
        content_serial: NonZeroU64,
        layers: Vec<PrivateFrame>,
    ) -> Result<Self, RejectedSceneFrames> {
        if !scene_serials_match(
            content_serial,
            layers.iter().map(PrivateFrame::content_serial),
        ) {
            return Err(RejectedSceneFrames {
                layers,
                cause: invalid("scene layers do not share its content identity"),
            });
        }
        Ok(Self {
            layers,
            content_serial,
        })
    }

    pub fn content_serial(&self) -> NonZeroU64 {
        self.content_serial
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn into_parts(self) -> (NonZeroU64, Vec<PrivateFrame>) {
        (self.content_serial, self.layers)
    }
}

/// Layer frames rejected without losing their private buffer owners.
pub struct RejectedSceneFrames {
    layers: Vec<PrivateFrame>,
    cause: io::Error,
}

impl std::fmt::Debug for RejectedSceneFrames {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedSceneFrames")
            .field("layer_count", &self.layers.len())
            .field("cause", &self.cause)
            .finish()
    }
}

impl std::fmt::Display for RejectedSceneFrames {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "reject complete scene frames: {}", self.cause)
    }
}

impl std::error::Error for RejectedSceneFrames {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl RejectedSceneFrames {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (Vec<PrivateFrame>, io::Error) {
        (self.layers, self.cause)
    }
}

fn scene_serials_match(
    content_serial: NonZeroU64,
    serials: impl Iterator<Item = Option<NonZeroU64>>,
) -> bool {
    serials
        .into_iter()
        .all(|serial| serial == Some(content_serial))
}

/// Buffers and immutable frame metadata for one scene execution.
pub struct SceneInputs {
    destination: PrivateBuffer,
    frames: SceneFrames,
    background: [u8; 3],
}

impl SceneInputs {
    pub fn new(destination: PrivateBuffer, frames: SceneFrames, background: [u8; 3]) -> Self {
        Self {
            destination,
            frames,
            background,
        }
    }

    pub fn into_parts(self) -> (PrivateBuffer, SceneFrames, [u8; 3]) {
        (self.destination, self.frames, self.background)
    }
}

/// Rejected inputs whose pixels and pool ownership remain unchanged.
pub struct RejectedScene {
    inputs: SceneInputs,
    cause: io::Error,
}

impl RejectedScene {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (SceneInputs, io::Error) {
        (self.inputs, self.cause)
    }
}

/// A scene rejected before native work or consumed by a native failure.
pub enum SceneCompositionError {
    Rejected(RejectedScene),
    Native(io::Error),
}

impl std::fmt::Debug for SceneCompositionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(scene) => formatter
                .debug_tuple("Rejected")
                .field(scene.cause())
                .finish(),
            Self::Native(error) => formatter.debug_tuple("Native").field(error).finish(),
        }
    }
}

impl std::fmt::Display for SceneCompositionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(scene) => write!(formatter, "reject scene inputs: {}", scene.cause()),
            Self::Native(error) => write!(formatter, "compose private scene: {error}"),
        }
    }
}

impl std::error::Error for SceneCompositionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(scene) => Some(scene.cause()),
            Self::Native(error) => Some(error),
        }
    }
}

/// Completed final frame and unchanged private source allocations.
pub struct ComposedFrame {
    sources: Vec<PrivateBuffer>,
    frame: PrivateFrame,
}

impl ComposedFrame {
    pub fn into_parts(self) -> (Vec<PrivateBuffer>, PrivateFrame) {
        (self.sources, self.frame)
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use drm_display_executor::scene::{
        color::{ColorOperation, ColorPipeline, OutputColor},
        geometry::SourceRect,
    };
    use pronk_gpu::vulkan::{LayerRequirements, PackedFormat, SourceRequirements};

    use super::*;
    use crate::SceneBuffers;

    #[test]
    fn opaque_sources_ignore_every_pixel_alpha_mode() {
        for pixel in [
            PixelBlend::None,
            PixelBlend::Premultiplied,
            PixelBlend::Coverage,
        ] {
            assert_eq!(
                effective_blend(
                    SourceAlpha::Opaque,
                    Blend {
                        pixel,
                        plane_alpha: 12345,
                    },
                ),
                Blend {
                    pixel: PixelBlend::None,
                    plane_alpha: 12345,
                }
            );
        }
    }

    #[test]
    fn alpha_channels_retain_the_selected_blend_mode() {
        for pixel in [
            PixelBlend::None,
            PixelBlend::Premultiplied,
            PixelBlend::Coverage,
        ] {
            let blend = Blend {
                pixel,
                plane_alpha: 12345,
            };
            assert_eq!(effective_blend(SourceAlpha::Channel, blend), blend);
        }
    }

    #[test]
    fn complete_scene_identity_rejects_missing_or_mixed_layer_serials() {
        let serial = NonZeroU64::new(73).unwrap();
        assert!(scene_serials_match(serial, std::iter::empty()));
        assert!(scene_serials_match(
            serial,
            [Some(serial), Some(serial)].into_iter()
        ));
        assert!(!scene_serials_match(serial, [None].into_iter()));
        assert!(!scene_serials_match(
            serial,
            [Some(serial), NonZeroU64::new(74)].into_iter()
        ));
    }

    fn device() -> (Device, u64) {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
        let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
        let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
        (Device::open(node).unwrap(), modifier)
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn qualified_scene_preserves_every_private_pool_owner() {
        let (device, modifier) = device();
        let extent = Extent::new(4, 3).unwrap();
        let source = SourceRequirements {
            format: PackedFormat::Bgra8,
            extent,
            modifier,
        };
        let layers = [LayerRequirements {
            source,
            crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
            destination: DestinationRect {
                position: [0, 0],
                extent,
            },
            transform: Transform::default(),
            blend: Blend {
                pixel: PixelBlend::Coverage,
                plane_alpha: 32768,
            },
            color: ColorPipeline::new(&[ColorOperation::SrgbEotf]),
        }];
        let composer = SceneComposer::new(
            &device,
            SceneRequirements {
                output: extent,
                layers: &layers,
                color: OutputColor::default(),
            },
        )
        .unwrap();
        assert_eq!(composer.output(), extent);
        assert_eq!(composer.layer_count(), 1);
        assert_eq!(composer.source_requirements(0), Some(source));
        assert_eq!(composer.source_requirements(1), None);
        let mut pool = composer
            .create_pool(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(2).unwrap())
            .unwrap();
        let SceneBuffers {
            destination,
            mut sources,
        } = pool.take().unwrap().unwrap();
        let mut source = sources
            .pop()
            .unwrap()
            .clear_and_wait([231, 57, 19])
            .unwrap();
        let content_serial = NonZeroU64::new(73).unwrap();
        source.content_serial = Some(content_serial);
        let frames = SceneFrames::new(content_serial, vec![source]).unwrap();
        let result = composer
            .compose_and_wait(SceneInputs::new(destination, frames, [17, 85, 204]))
            .unwrap();
        let (sources, frame) = result.into_parts();
        assert_eq!(frame.content_serial(), NonZeroU64::new(73));
        assert_eq!(frame.source_alpha(), SourceAlpha::Opaque);
        assert_eq!(sources.len(), 1);
        assert!(pool.restore_sources(sources).is_ok());
        assert!(pool.restore_destination(frame.buffer).is_ok());
        assert_eq!(pool.available(), 2);
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn mixed_scene_identity_returns_every_private_owner() {
        let (device, _) = device();
        let extent = Extent::new(4, 3).unwrap();
        let mut pool = ScenePool::new(
            &device,
            extent,
            [extent, extent].into_iter(),
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &Arc::new(()),
        )
        .unwrap();
        let SceneBuffers {
            destination,
            sources,
        } = pool.take().unwrap().unwrap();
        let serial = NonZeroU64::new(73).unwrap();
        let mut frames: Vec<_> = sources
            .into_iter()
            .map(|source| source.clear_and_wait([17, 85, 204]).unwrap())
            .collect();
        frames[0].content_serial = Some(serial);
        frames[1].content_serial = NonZeroU64::new(74);

        let rejected = SceneFrames::new(serial, frames).err().unwrap();
        assert_eq!(rejected.cause().kind(), io::ErrorKind::InvalidInput);
        let (frames, _) = rejected.into_parts();
        let sources = frames.into_iter().map(|frame| frame.buffer).collect();
        assert!(pool
            .restore(SceneBuffers {
                destination,
                sources,
            })
            .is_ok());
        assert_eq!(pool.available(), 1);
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn equal_extent_layer_swaps_are_rejected_before_native_work() {
        let (device, modifier) = device();
        let extent = Extent::new(4, 3).unwrap();
        let source = SourceRequirements {
            format: PackedFormat::Bgra8,
            extent,
            modifier,
        };
        let layers = [
            LayerRequirements {
                source,
                crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
                destination: DestinationRect {
                    position: [0, 0],
                    extent,
                },
                transform: Transform::default(),
                blend: Blend::default(),
                color: ColorPipeline::new(&[]),
            },
            LayerRequirements {
                source,
                crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
                destination: DestinationRect {
                    position: [1, 0],
                    extent,
                },
                transform: Transform::default(),
                blend: Blend::default(),
                color: ColorPipeline::new(&[]),
            },
        ];
        let composer = SceneComposer::new(
            &device,
            SceneRequirements {
                output: extent,
                layers: &layers,
                color: OutputColor::default(),
            },
        )
        .unwrap();
        let mut pool = composer
            .create_pool(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap())
            .unwrap();
        let SceneBuffers {
            destination,
            sources,
        } = pool.take().unwrap().unwrap();
        let serial = NonZeroU64::new(73).unwrap();
        let mut frames: Vec<_> = sources
            .into_iter()
            .map(|source| {
                let mut frame = source.clear_and_wait([17, 85, 204]).unwrap();
                frame.content_serial = Some(serial);
                frame
            })
            .collect();
        frames.swap(0, 1);
        let frames = SceneFrames::new(serial, frames).unwrap();

        let error = composer
            .compose_and_wait(SceneInputs::new(destination, frames, [0; 3]))
            .err()
            .unwrap();
        let SceneCompositionError::Rejected(rejected) = error else {
            panic!("layer role mismatch reached native work");
        };
        let (inputs, _) = rejected.into_parts();
        let (destination, frames, _) = inputs.into_parts();
        let (_, mut frames) = frames.into_parts();
        frames.swap(0, 1);
        let sources = frames.into_iter().map(|frame| frame.buffer).collect();
        assert!(pool
            .restore(SceneBuffers {
                destination,
                sources,
            })
            .is_ok());
        assert_eq!(pool.available(), 1);
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn private_storage_is_shared_only_through_its_explicit_profile() {
        let (device, modifier) = device();
        let extent = Extent::new(4, 3).unwrap();
        assert!(SceneStorageProfile::new(
            &device,
            extent,
            &[SourceRequirements {
                format: PackedFormat::Bgra8,
                extent,
                modifier: u64::MAX,
            }],
        )
        .is_err());
        let source = SourceRequirements {
            format: PackedFormat::Bgra8,
            extent,
            modifier,
        };
        let layers = [LayerRequirements {
            source,
            crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
            destination: DestinationRect {
                position: [0, 0],
                extent,
            },
            transform: Transform::default(),
            blend: Blend::default(),
            color: ColorPipeline::new(&[]),
        }];
        let scene = SceneRequirements {
            output: extent,
            layers: &layers,
            color: OutputColor::default(),
        };
        let first = SceneComposer::new(&device, scene).unwrap();
        let second = SceneComposer::new(&device, scene).unwrap();
        let mut foreign_pool = second
            .create_pool(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap())
            .unwrap();
        let SceneBuffers {
            destination,
            mut sources,
        } = foreign_pool.take().unwrap().unwrap();
        let serial = NonZeroU64::new(73).unwrap();
        let mut source = sources
            .pop()
            .unwrap()
            .clear_and_wait([17, 85, 204])
            .unwrap();
        source.content_serial = Some(serial);
        let frames = SceneFrames::new(serial, vec![source]).unwrap();

        let error = first
            .compose_and_wait(SceneInputs::new(destination, frames, [0; 3]))
            .err()
            .unwrap();
        let SceneCompositionError::Rejected(rejected) = error else {
            panic!("foreign profile storage reached native work");
        };
        let (inputs, _) = rejected.into_parts();
        let (destination, frames, _) = inputs.into_parts();
        let (_, frames) = frames.into_parts();
        let sources = frames.into_iter().map(|frame| frame.buffer).collect();
        assert!(foreign_pool
            .restore(SceneBuffers {
                destination,
                sources,
            })
            .is_ok());
        assert_eq!(foreign_pool.available(), 1);

        let moved_layers = [LayerRequirements {
            destination: DestinationRect {
                position: [1, 0],
                extent,
            },
            ..layers[0]
        }];
        let moved_scene = SceneRequirements {
            output: extent,
            layers: &moved_layers,
            color: OutputColor::default(),
        };
        let shared = SceneComposer::with_storage(first.storage(), moved_scene).unwrap();
        let mut pool = first
            .create_pool(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap())
            .unwrap();
        let SceneBuffers {
            destination,
            mut sources,
        } = pool.take().unwrap().unwrap();
        let mut source = sources
            .pop()
            .unwrap()
            .clear_and_wait([17, 85, 204])
            .unwrap();
        source.content_serial = Some(serial);
        let frames = SceneFrames::new(serial, vec![source]).unwrap();
        let composed = shared
            .compose_and_wait(SceneInputs::new(destination, frames, [0; 3]))
            .unwrap();
        let rejected = foreign_pool.finish_composition(composed).err().unwrap();
        let (sources, destination) = rejected.into_parts();
        assert!(pool.restore_sources(sources).is_ok());
        assert!(pool.restore_destination(destination.buffer).is_ok());
        assert_eq!(pool.available(), 1);
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn rejected_scene_returns_untouched_inputs() {
        let (device, modifier) = device();
        let extent = Extent::new(1, 1).unwrap();
        let source = SourceRequirements {
            format: PackedFormat::Bgra8,
            extent,
            modifier,
        };
        let layers = [LayerRequirements {
            source,
            crop: SourceRect::new(extent, [0, 0], extent).unwrap(),
            destination: DestinationRect {
                position: [0, 0],
                extent,
            },
            transform: Transform::default(),
            blend: Blend::default(),
            color: ColorPipeline::new(&[]),
        }];
        let composer = SceneComposer::new(
            &device,
            SceneRequirements {
                output: extent,
                layers: &layers,
                color: OutputColor::default(),
            },
        )
        .unwrap();
        let mut pool = crate::PrivatePool::new(
            &device,
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
        )
        .unwrap();
        let frames = SceneFrames::new(NonZeroU64::new(1).unwrap(), Vec::new()).unwrap();
        let error =
            match composer.compose_and_wait(SceneInputs::new(pool.take().unwrap(), frames, [0; 3]))
            {
                Ok(_) => panic!("source-count mismatch was accepted"),
                Err(error) => error,
            };
        let SceneCompositionError::Rejected(rejected) = error else {
            panic!("input mismatch reached native work");
        };
        assert_eq!(rejected.cause().kind(), io::ErrorKind::InvalidInput);
        let (inputs, _) = rejected.into_parts();
        let (destination, frames, _) = inputs.into_parts();
        assert!(frames.is_empty());
        assert!(pool.put(destination).is_ok());
        assert_eq!(pool.available(), 1);
    }
}
