//! Offline CPU scene replay; deliberately independent of device access and UAPI.

#![forbid(unsafe_code)]

mod fixture;
mod ppm;

use anyhow::{bail, ensure, Context, Result};
use drm_display_executor::render::cpu::{
    compose::{compose_with_output_color, Layer},
    image::{Image, ImageMut, LinearLayout},
};
use drm_display_executor::scene::{
    blend::Blend,
    color::{Lut, OutputColor},
    format::PackedRgbFormat,
    geometry::{Extent, SourceRect},
    transform::{Rotation, Transform},
};

/// Fixture input bound, independent of any kernel or production queue limit.
pub const MAX_FIXTURE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Completed opaque RGBA pixels, with no native allocation or capture identity.
pub struct Rendered {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Decode and render one bounded, versioned offline scene fixture.
pub fn render(input: &[u8]) -> Result<Rendered> {
    ensure!(
        input.len() <= MAX_FIXTURE_BYTES,
        "fixture exceeds input limit"
    );
    let fixture: fixture::Fixture =
        serde_json::from_slice(input).context("decode scene fixture")?;
    ensure!(fixture.version == 1, "unsupported fixture version");
    ensure!(fixture.sources.len() <= 64, "too many fixture sources");
    ensure!(fixture.layers.len() <= 256, "too many fixture layers");
    let color = OutputColor {
        gamma: fixture.output.gamma.as_deref().map(Lut::new).transpose()?,
    };
    let extent = Extent::new(fixture.output.width, fixture.output.height)?;
    let stride = usize::try_from(extent.width())?
        .checked_mul(4)
        .context("output stride overflow")?;
    let layout = LinearLayout::new(extent, PackedRgbFormat::Abgr8888, 0, stride)?;
    ensure!(
        layout.required_bytes() <= MAX_OUTPUT_BYTES,
        "output exceeds pixel limit"
    );

    let images = fixture
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let layout = LinearLayout::new(
                Extent::new(source.width, source.height)?,
                source.format.into(),
                source.offset,
                source.stride,
            )?;
            Image::new(&source.bytes, layout).with_context(|| format!("source {index} storage"))
        })
        .collect::<Result<Vec<_>>>()?;
    let layers = fixture
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| -> Result<_> {
            let image = *images
                .get(layer.source)
                .with_context(|| format!("layer {index} source"))?;
            let crop = SourceRect::from_fixed_16_16(image.layout().extent(), layer.crop_16_16)
                .with_context(|| format!("layer {index} crop"))?;
            let rotation = match layer.rotation {
                0 => Rotation::Rotate0,
                90 => Rotation::Rotate90,
                180 => Rotation::Rotate180,
                270 => Rotation::Rotate270,
                _ => bail!("layer {index} rotation is not orthogonal"),
            };
            Ok(
                Layer::new(image, crop.origin(), crop.extent(), layer.position)?
                    .with_blend(Blend {
                        pixel: layer.pixel_blend.into(),
                        plane_alpha: layer.plane_alpha,
                    })
                    .with_transform(Transform {
                        rotation,
                        reflect_x: layer.reflect_x,
                        reflect_y: layer.reflect_y,
                    }),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(layout.required_bytes())?;
    pixels.resize(layout.required_bytes(), 0);
    compose_with_output_color(
        &mut ImageMut::new(&mut pixels, layout)?,
        fixture.output.background,
        &layers,
        color,
    )?;
    Ok(Rendered {
        width: extent.width(),
        height: extent.height(),
        pixels,
    })
}
