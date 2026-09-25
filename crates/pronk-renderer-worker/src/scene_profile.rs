//! Adapt checked CastKMS scene metadata into a qualified native profile.

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::fd::AsFd;

use castkms_renderer::{
    ColorOperation as WireColor, ConstraintsFormat, FormatModifier, RendererConstraints, SceneJob,
    StorageProvenance,
};
use castkms_sys::{
    DRM_FORMAT_ABGR2101010, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB2101010, DRM_FORMAT_ARGB8888,
    DRM_FORMAT_RGB565, DRM_FORMAT_XBGR2101010, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB2101010,
    DRM_FORMAT_XRGB8888, RENDERER_CONSTRAINTS_MAX_FORMATS,
};
use drm_display_executor::scene::{
    blend::{Blend, PixelBlend},
    color::{ColorMatrix, ColorOperation, ColorPipeline, Lut, OutputColor},
    transform::Transform,
};
use pronk_gpu::vulkan::{Device, PackedFormat};
use pronk_gpu::vulkan::{LayerRequirements, SceneRequirements, SourceRequirements};

use crate::pool::{MAX_PRIVATE_BUFFERS, MAX_PRIVATE_POOL_BYTES, PRIVATE_PIXEL_BYTES};
use crate::source::{explicit_modifier, packed_format};
use crate::{SceneComposer, ScenePool, SceneStorageProfile};

/// One advertised primary-plane contract paired with its private storage policy.
pub struct PrimarySceneProfile {
    constraints: RendererConstraints,
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
                if !append_source_layout(
                    &mut formats,
                    &mut sources,
                    packed,
                    fourccs,
                    modifier,
                    output,
                )? {
                    break;
                }
            }
        }
        let constraints =
            RendererConstraints::single_primary_formats(output, formats.into_boxed_slice())?
                .with_output_color(256, true)?;
        let storage =
            SceneStorageProfile::single_primary(device, output, sources.into_boxed_slice())?;
        Ok(Self {
            constraints,
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

    /// Bound one primary scene's final and source storage before allocating it.
    ///
    /// Leave one eighth of the private-pool limit for native image rounding.
    /// The allocator's actual byte counts are checked again by `create_pool`.
    pub fn bounded_capacities(
        &self,
        maximum_final: NonZeroUsize,
        maximum_source: NonZeroUsize,
    ) -> io::Result<(NonZeroUsize, NonZeroUsize)> {
        bounded_capacities(self.storage.output(), maximum_final, maximum_source)
    }

    /// Release the kernel declaration and matching private-storage policy.
    pub fn into_parts(self) -> (RendererConstraints, SceneStorageProfile) {
        (self.constraints, self.storage)
    }
}

fn bounded_capacities(
    output: drm_display_executor::scene::geometry::Extent,
    maximum_final: NonZeroUsize,
    maximum_source: NonZeroUsize,
) -> io::Result<(NonZeroUsize, NonZeroUsize)> {
    if maximum_final.get() > MAX_PRIVATE_BUFFERS || maximum_source.get() > MAX_PRIVATE_BUFFERS {
        return Err(invalid("private scene capacity exceeds its buffer limit"));
    }
    let bytes_per_image = u64::from(output.width())
        .checked_mul(u64::from(output.height()))
        .and_then(|pixels| pixels.checked_mul(PRIVATE_PIXEL_BYTES))
        .ok_or_else(|| invalid("private scene image size overflowed"))?;
    let budget = MAX_PRIVATE_POOL_BYTES - MAX_PRIVATE_POOL_BYTES / 8;
    let slots = usize::try_from(budget / bytes_per_image)
        .map_err(|_| invalid("private scene capacity exceeds the host size range"))?;
    let final_count = maximum_final.get().min(slots.saturating_sub(1));
    let source_count = maximum_source.get().min(slots.saturating_sub(final_count));
    let final_count = NonZeroUsize::new(final_count)
        .ok_or_else(|| invalid("private scene has no final-image capacity"))?;
    let source_count = NonZeroUsize::new(source_count)
        .ok_or_else(|| invalid("private scene has no source-image capacity"))?;
    Ok((final_count, source_count))
}

fn append_source_layout(
    formats: &mut Vec<ConstraintsFormat>,
    sources: &mut Vec<SourceRequirements>,
    packed: PackedFormat,
    fourccs: &[u32],
    modifier: u64,
    output: drm_display_executor::scene::geometry::Extent,
) -> io::Result<bool> {
    let record_count = fourccs.len();
    if formats.len().saturating_add(record_count) > RENDERER_CONSTRAINTS_MAX_FORMATS {
        return Ok(false);
    }
    let mut records = Vec::new();
    records
        .try_reserve_exact(record_count)
        .map_err(io::Error::other)?;
    let pixel_bytes =
        NonZeroU32::new(packed.bytes_per_pixel()).expect("packed pixels occupy nonzero storage");
    for &fourcc in fourccs {
        records.push(ConstraintsFormat::new(
            fourcc,
            FormatModifier::Explicit(modifier),
            NonZeroU32::new(1).expect("one source plane is nonzero"),
            StorageProvenance::Imported,
            pixel_bytes,
            pixel_bytes,
            NonZeroU32::new(u32::MAX).expect("maximum pitch is nonzero"),
        )?);
    }
    formats
        .try_reserve_exact(records.len())
        .map_err(io::Error::other)?;
    sources.try_reserve(1).map_err(io::Error::other)?;
    formats.extend(records);
    sources.push(SourceRequirements {
        format: packed,
        extent: output,
        modifier,
    });
    Ok(true)
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
        job: &SceneJob<'_, F>,
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
            let modifier = explicit_modifier(image.modifier())?;
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

    fn capacity(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[test]
    fn scene_capacity_preserves_depth_until_four_k_needs_a_smaller_pool() {
        let below = drm_display_executor::scene::geometry::Extent::new(2560, 1440).unwrap();
        assert_eq!(
            bounded_capacities(below, capacity(3), capacity(3)).unwrap(),
            (capacity(3), capacity(3))
        );
        let four_k = drm_display_executor::scene::geometry::Extent::new(3840, 2160).unwrap();
        assert_eq!(
            bounded_capacities(four_k, capacity(3), capacity(3)).unwrap(),
            (capacity(2), capacity(1))
        );
        let too_large = drm_display_executor::scene::geometry::Extent::new(7680, 4320).unwrap();
        assert!(bounded_capacities(too_large, capacity(3), capacity(3)).is_err());
    }

    #[test]
    #[ignore = "requires an explicitly selected Vulkan render node and native memory"]
    fn selected_gpu_allocates_the_bounded_four_k_scene_pool() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").unwrap();
        let device = Device::open(node).unwrap();
        let output = drm_display_executor::scene::geometry::Extent::new(3840, 2160).unwrap();
        let profile = PrimarySceneProfile::discover(&device, output).unwrap();
        let (final_images, sources) = profile
            .bounded_capacities(capacity(3), capacity(3))
            .unwrap();
        assert_eq!((final_images, sources), (capacity(2), capacity(1)));
        let _pool = profile.create_pool(final_images, sources).unwrap();
        let modifiers = device
            .private_storage_modifiers(
                PackedFormat::Bgra8,
                NonZeroU32::new(output.width()).unwrap(),
                NonZeroU32::new(output.height()).unwrap(),
            )
            .unwrap();
        let modifier = modifiers.first().copied().unwrap();
        let _packed = crate::PreparedSceneImages::new(
            &device,
            NonZeroU32::new(output.width()).unwrap(),
            NonZeroU32::new(output.height()).unwrap(),
            PackedFormat::Bgra8,
            modifier,
            final_images,
        )
        .unwrap();
    }

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
            profile.constraints.formats().iter().any(|format| {
                source_formats()
                    .into_iter()
                    .find(|(packed, _)| *packed == source.format)
                    .is_some_and(|(_, fourccs)| fourccs.contains(&format.fourcc()))
                    && format.modifier() == FormatModifier::Explicit(source.modifier)
            })
        }));
    }

    #[test]
    fn source_discovery_never_partially_exceeds_the_wire_record_budget() {
        let output = drm_display_executor::scene::geometry::Extent::new(1920, 1080).unwrap();
        let mut formats = Vec::new();
        let mut sources = Vec::new();
        for modifier in 0..=RENDERER_CONSTRAINTS_MAX_FORMATS as u64 {
            if !append_source_layout(
                &mut formats,
                &mut sources,
                PackedFormat::Bgra8,
                &[DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888],
                modifier,
                output,
            )
            .unwrap()
            {
                break;
            }
        }

        assert_eq!(formats.len(), RENDERER_CONSTRAINTS_MAX_FORMATS);
        assert_eq!(sources.len(), 128);
        assert_eq!(sources.last().unwrap().modifier, 127);
        assert!(formats.iter().all(|format| {
            matches!(format.modifier(), FormatModifier::Explicit(_))
                && !format.provenance().native()
                && format.provenance().imported()
                && format.pitch_alignment().get() == 4
                && format.offset_alignment().get() == 4
        }));
    }
}
