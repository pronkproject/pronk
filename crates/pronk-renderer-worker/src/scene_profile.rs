//! Adapt checked CastKMS scene metadata into a qualified native profile.

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::AsFd;

use castkms_renderer::{
    CapabilityFormat, CapabilityProfile, ColorOperation as WireColor, FormatModifier,
    ProfileRegistrationError, RegisteredCandidate, RendererCapability, SceneJob, StorageProvenance,
    TakeoverCandidate,
};
use castkms_sys::{
    DRM_FORMAT_ABGR2101010, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_RGB565, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XRGB8888,
};
use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    color::{ColorMatrix, ColorOperation, ColorPipeline, Lut, OutputColor},
    transform::Transform,
};
use pronk_gpu::vulkan::{Device, PackedFormat};
use pronk_gpu::vulkan::{LayerRequirements, SceneRequirements, SourceRequirements};

use crate::source::packed_format;
use crate::{SceneComposer, ScenePool, SceneStorageProfile};

/// One advertised primary-plane contract paired with its private storage policy.
pub struct PrimarySceneProfile {
    capability: RendererCapability,
    storage: SceneStorageProfile,
}

impl PrimarySceneProfile {
    /// Discover every exact packed source layout accepted by the selected GPU.
    pub fn discover(
        device: &Device,
        output: drm_display_executor::scene::geometry::Extent,
    ) -> io::Result<Self> {
        let width = NonZeroU32::new(output.width()).expect("scene output width is nonzero");
        let height = NonZeroU32::new(output.height()).expect("scene output height is nonzero");
        let mut formats = Vec::new();
        let mut sources = Vec::new();
        for (packed, fourccs) in source_formats() {
            for modifier in device.source_modifiers(packed, width, height)? {
                sources.push(SourceRequirements {
                    format: packed,
                    extent: output,
                    modifier,
                });
                for &fourcc in fourccs {
                    formats.push(CapabilityFormat::new(
                        fourcc,
                        FormatModifier::Explicit(modifier),
                        NonZeroU32::new(1).expect("one source plane is nonzero"),
                        StorageProvenance::new(true, true),
                        NonZeroU32::new(1).expect("unit pitch alignment is nonzero"),
                        NonZeroU32::new(1).expect("unit offset alignment is nonzero"),
                        NonZeroU32::new(u32::MAX).expect("maximum pitch is nonzero"),
                    )?);
                }
            }
        }
        let capability =
            RendererCapability::single_primary_formats(output, formats.into_boxed_slice())?;
        let storage =
            SceneStorageProfile::single_primary(device, output, sources.into_boxed_slice())?;
        Ok(Self {
            capability,
            storage,
        })
    }

    pub fn create_pool(
        &self,
        final_capacity: NonZeroUsize,
        source_capacity: NonZeroUsize,
    ) -> io::Result<ScenePool> {
        self.storage.create_pool(final_capacity, source_capacity)
    }

    /// Register the advertised contract and release its matching storage policy.
    pub fn register<'renderer, F: AsFd>(
        self,
        candidate: TakeoverCandidate<'renderer, F>,
    ) -> Result<
        (RegisteredCandidate<'renderer, F>, SceneStorageProfile),
        ProfileRegistrationError<'renderer, F>,
    > {
        candidate
            .register_profile(&CapabilityProfile::Renderer(self.capability))
            .map(|candidate| (candidate, self.storage))
    }
}

fn source_formats() -> [(PackedFormat, &'static [u32]); 5] {
    [
        (
            PackedFormat::Bgra8,
            &[DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888],
        ),
        (
            PackedFormat::Rgba8,
            &[DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888],
        ),
        (
            PackedFormat::Bgr10A2,
            &[DRM_FORMAT_XRGB2101010, DRM_FORMAT_ARGB2101010],
        ),
        (
            PackedFormat::Rgb10A2,
            &[DRM_FORMAT_XBGR2101010, DRM_FORMAT_ABGR2101010],
        ),
        (PackedFormat::Rgb565, &[DRM_FORMAT_RGB565]),
    ]
}

impl SceneComposer {
    /// Qualify one checked complete-scene job for native execution.
    pub(crate) fn from_scene_job<F: AsFd>(
        storage: &SceneStorageProfile,
        job: &SceneJob<'_, '_, F>,
    ) -> io::Result<Self> {
        let mut colors = Vec::new();
        colors
            .try_reserve_exact(job.layers().len())
            .map_err(io::Error::other)?;
        for layer in job.layers() {
            colors.push(layer_color(layer.color())?);
        }
        let mut layers = Vec::new();
        layers
            .try_reserve_exact(job.layers().len())
            .map_err(io::Error::other)?;
        for (layer, color) in job.layers().iter().zip(&colors) {
            let image = layer.image();
            let format = packed_format(image.format())?;
            let modifier = match image.modifier() {
                FormatModifier::Explicit(modifier) => modifier,
                FormatModifier::Unspecified => {
                    return Err(unsupported("scene layer has no explicit format modifier"));
                }
            };
            layers.push(LayerRequirements {
                source: SourceRequirements {
                    format,
                    extent: image.extent(),
                    modifier,
                },
                crop: layer.source(),
                destination: layer.destination(),
                transform: Transform::default(),
                blend: Blend {
                    pixel: PixelBlend::Premultiplied,
                    plane_alpha: u16::MAX,
                },
                color: ColorPipeline::new(color),
            });
        }
        Self::with_storage(
            storage,
            SceneRequirements {
                output: job.output(),
                layers: &layers,
                color: output_color(job.color())?,
            },
        )
    }
}

fn layer_color(operations: &[WireColor]) -> io::Result<Vec<ColorOperation<'_>>> {
    let mut color = Vec::new();
    color
        .try_reserve_exact(operations.len())
        .map_err(io::Error::other)?;
    for operation in operations {
        color.push(match operation {
            WireColor::Bypass => ColorOperation::Bypass,
            WireColor::SrgbEotf => ColorOperation::SrgbEotf,
            WireColor::SrgbInverseEotf => ColorOperation::SrgbInverseEotf,
            WireColor::Matrix(matrix) => {
                ColorOperation::Matrix(ColorMatrix::from_sign_magnitude(*matrix))
            }
            WireColor::Lut(entries) => ColorOperation::Lut(
                Lut::new(entries)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            ),
        });
    }
    Ok(color)
}

fn output_color(operations: &[WireColor]) -> io::Result<OutputColor<'_>> {
    match operations {
        [] => Ok(OutputColor::default()),
        [WireColor::Matrix(_)] => Ok(OutputColor {
            matrix: Some(output_matrix(&operations[0])?),
            ..OutputColor::default()
        }),
        [WireColor::Lut(_)] => Ok(OutputColor {
            gamma: Some(output_lut(&operations[0])?),
            ..OutputColor::default()
        }),
        [WireColor::Lut(_), WireColor::Matrix(_)] => Ok(OutputColor {
            degamma: Some(output_lut(&operations[0])?),
            matrix: Some(output_matrix(&operations[1])?),
            gamma: None,
        }),
        [WireColor::Matrix(_), WireColor::Lut(_)] => Ok(OutputColor {
            degamma: None,
            matrix: Some(output_matrix(&operations[0])?),
            gamma: Some(output_lut(&operations[1])?),
        }),
        [WireColor::Lut(_), WireColor::Lut(_)] => Ok(OutputColor {
            degamma: Some(output_lut(&operations[0])?),
            matrix: None,
            gamma: Some(output_lut(&operations[1])?),
        }),
        [WireColor::Lut(_), WireColor::Matrix(_), WireColor::Lut(_)] => Ok(OutputColor {
            degamma: Some(output_lut(&operations[0])?),
            matrix: Some(output_matrix(&operations[1])?),
            gamma: Some(output_lut(&operations[2])?),
        }),
        _ => Err(invalid("output color operations are outside display order")),
    }
}

fn output_lut(operation: &WireColor) -> io::Result<Lut<'_>> {
    match operation {
        WireColor::Lut(entries) => {
            Lut::new(entries).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        _ => Err(invalid("output color stage is not a lookup table")),
    }
}

fn output_matrix(operation: &WireColor) -> io::Result<ColorMatrix> {
    match operation {
        WireColor::Matrix(matrix) => Ok(ColorMatrix::from_sign_magnitude(*matrix)),
        _ => Err(invalid("output color stage is not a matrix")),
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lut(entries: &[[u16; 3]]) -> WireColor {
        WireColor::Lut(entries.to_vec().into_boxed_slice())
    }

    fn matrix() -> WireColor {
        WireColor::Matrix([0; 12])
    }

    #[test]
    fn output_color_requires_unambiguous_stage_positions() {
        let table = [[0, 1, 2], [u16::MAX; 3]];
        for operations in [vec![], vec![matrix()]] {
            assert!(output_color(&operations).is_ok());
        }
        let one_lut = [lut(&table)];
        let color = output_color(&one_lut).unwrap();
        assert!(color.degamma.is_none());
        assert!(color.matrix.is_none());
        assert!(color.gamma.is_some());
        let degamma_matrix = [lut(&table), matrix()];
        let color = output_color(&degamma_matrix).unwrap();
        assert!(color.degamma.is_some());
        assert!(color.matrix.is_some());
        assert!(color.gamma.is_none());
        let matrix_gamma = [matrix(), lut(&table)];
        let color = output_color(&matrix_gamma).unwrap();
        assert!(color.degamma.is_none());
        assert!(color.matrix.is_some());
        assert!(color.gamma.is_some());
        let both_luts = [lut(&table), lut(&table)];
        let color = output_color(&both_luts).unwrap();
        assert!(color.degamma.is_some());
        assert!(color.matrix.is_none());
        assert!(color.gamma.is_some());
        let complete = [lut(&table), matrix(), lut(&table)];
        let color = output_color(&complete).unwrap();
        assert!(color.degamma.is_some());
        assert!(color.matrix.is_some());
        assert!(color.gamma.is_some());
        assert_eq!(
            output_color(&[matrix(), matrix()]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn layer_color_preserves_order_and_owned_payloads() {
        let table = [[0, 1, 2], [u16::MAX; 3]];
        let wire = [
            WireColor::SrgbEotf,
            matrix(),
            lut(&table),
            WireColor::SrgbInverseEotf,
        ];
        let color = layer_color(&wire).unwrap();
        assert!(matches!(color[0], ColorOperation::SrgbEotf));
        assert!(matches!(color[1], ColorOperation::Matrix(_)));
        assert!(matches!(color[2], ColorOperation::Lut(_)));
        assert!(matches!(color[3], ColorOperation::SrgbInverseEotf));
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU selection"]
    fn discovered_primary_contract_matches_private_storage_options() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").unwrap();
        let device = Device::open(node).unwrap();
        let output = drm_display_executor::scene::geometry::Extent::new(1920, 1080).unwrap();
        let profile = PrimarySceneProfile::discover(&device, output).unwrap();
        let options = profile.storage.source_options(0).unwrap();

        assert!(!options.is_empty());
        assert!(options.iter().all(|source| {
            profile.capability.formats().iter().any(|format| {
                source_formats()
                    .into_iter()
                    .find(|(packed, _)| *packed == source.format)
                    .is_some_and(|(_, fourccs)| fourccs.contains(&format.fourcc()))
                    && format.modifier() == FormatModifier::Explicit(source.modifier)
            })
        }));
    }
}
