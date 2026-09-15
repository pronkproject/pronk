//! Complete private-scene execution after compositor sources retire.

use std::io;
use std::num::NonZeroU64;

use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    geometry::{DestinationRect, Extent, SourceRect},
    transform::Transform,
};
use pronk_gpu::vulkan::{Blender, Device, OutputColorProgram, PrivateLayer, SceneRequirements};

use crate::{PrivateBuffer, PrivateFrame, SourceAlpha};

#[derive(Clone, Copy)]
struct LayerPlan {
    source: Extent,
    crop: SourceRect,
    destination: DestinationRect,
    transform: Transform,
    blend: Blend,
}

/// Prepared execution state for one qualified whole-scene profile.
pub struct SceneComposer {
    device: Device,
    blender: Blender,
    color: OutputColorProgram,
    output: Extent,
    layers: Vec<LayerPlan>,
}

impl SceneComposer {
    /// Qualify and prepare one complete scene before source acquisition.
    pub fn new(device: &Device, scene: SceneRequirements<'_>) -> io::Result<Self> {
        let blender = device.create_blender()?;
        blender.check_scene(scene)?;
        let color = device.create_output_color(scene.output, scene.color)?;
        let mut layers = Vec::new();
        layers
            .try_reserve_exact(scene.layers.len())
            .map_err(io::Error::other)?;
        layers.extend(scene.layers.iter().map(|layer| LayerPlan {
            source: layer.source.extent,
            crop: layer.crop,
            destination: layer.destination,
            transform: layer.transform,
            blend: layer.blend,
        }));
        Ok(Self {
            device: device.clone(),
            blender,
            color,
            output: scene.output,
            layers,
        })
    }

    pub fn output(&self) -> Extent {
        self.output
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Compose qualified source frames and apply the complete output color path.
    ///
    /// Sources must be supplied in the profile's bottom-to-top order. Their
    /// compositor-source access has already ended; only independent private
    /// allocations enter this operation. Predictable input mismatches return
    /// every buffer untouched. A native failure consumes all affected buffers
    /// because their pixel validity may be unknown.
    pub fn compose_waited(
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
        let capacity = inputs.sources.len();
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
            sources,
            background,
            content_serial,
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
            .compose_waited(destination, background, layers)
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
            != (nonzero(self.output.width()), nonzero(self.output.height()))
            || !inputs.destination.is_owned_by(&self.device)
        {
            return Err(invalid(
                "scene destination does not match its qualified output",
            ));
        }
        if inputs.sources.len() != self.layers.len() {
            return Err(invalid(
                "scene source count does not match its qualified layers",
            ));
        }
        for (source, plan) in inputs.sources.iter().zip(&self.layers) {
            if source.extent() != (nonzero(plan.source.width()), nonzero(plan.source.height()))
                || !source.is_owned_by(&self.device)
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

/// Buffers and immutable frame metadata for one scene execution.
pub struct SceneInputs {
    pub destination: PrivateBuffer,
    pub sources: Vec<PrivateFrame>,
    pub background: [u8; 3],
    pub content_serial: NonZeroU64,
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

    use drm_display_executor::scene::{color::OutputColor, geometry::SourceRect};
    use pronk_gpu::vulkan::{LayerRequirements, PackedFormat, SourceRequirements};

    use super::*;

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
        let mut pool = crate::PrivatePool::new(
            &device,
            NonZeroU32::new(4).unwrap(),
            NonZeroU32::new(3).unwrap(),
            NonZeroUsize::new(2).unwrap(),
        )
        .unwrap();
        let source = pool.take().unwrap().clear_waited([231, 57, 19]).unwrap();
        let destination = pool.take().unwrap();
        let result = composer
            .compose_waited(SceneInputs {
                destination,
                sources: vec![source],
                background: [17, 85, 204],
                content_serial: NonZeroU64::new(73).unwrap(),
            })
            .unwrap();
        let (sources, frame) = result.into_parts();
        assert_eq!(frame.content_serial(), NonZeroU64::new(73));
        assert_eq!(frame.source_alpha(), SourceAlpha::Opaque);
        assert_eq!(sources.len(), 1);
        assert!(pool.put(sources.into_iter().next().unwrap()).is_ok());
        assert!(pool.put(frame.buffer).is_ok());
        assert_eq!(pool.available(), 2);
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
        let error = match composer.compose_waited(SceneInputs {
            destination: pool.take().unwrap(),
            sources: Vec::new(),
            background: [0; 3],
            content_serial: NonZeroU64::new(1).unwrap(),
        }) {
            Ok(_) => panic!("source-count mismatch was accepted"),
            Err(error) => error,
        };
        let SceneCompositionError::Rejected(rejected) = error else {
            panic!("input mismatch reached native work");
        };
        assert_eq!(rejected.cause().kind(), io::ErrorKind::InvalidInput);
        let (inputs, _) = rejected.into_parts();
        assert!(pool.put(inputs.destination).is_ok());
        assert_eq!(pool.available(), 1);
    }
}
