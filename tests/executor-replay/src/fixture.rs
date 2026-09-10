//! Offline fixture decoding, not an executor or capture wire protocol.

use drm_display_executor::scene::{blend::PixelBlend, format::PackedRgbFormat};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fixture {
    pub version: u32,
    pub output: Output,
    pub sources: Vec<Source>,
    pub layers: Vec<Layer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Output {
    pub width: u32,
    pub height: u32,
    pub background: [u8; 3],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub offset: usize,
    pub stride: usize,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) enum Format {
    #[serde(rename = "XR24")]
    Xrgb,
    #[serde(rename = "AR24")]
    Argb,
    #[serde(rename = "XB24")]
    Xbgr,
    #[serde(rename = "AB24")]
    Abgr,
}

impl From<Format> for PackedRgbFormat {
    fn from(value: Format) -> Self {
        match value {
            Format::Xrgb => Self::Xrgb8888,
            Format::Argb => Self::Argb8888,
            Format::Xbgr => Self::Xbgr8888,
            Format::Abgr => Self::Abgr8888,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Layer {
    pub source: usize,
    pub crop_16_16: [u32; 4],
    pub position: [i32; 2],
    pub pixel_blend: Blend,
    pub plane_alpha: u16,
    pub rotation: u16,
    pub reflect_x: bool,
    pub reflect_y: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Blend {
    None,
    Premultiplied,
    Coverage,
}

impl From<Blend> for PixelBlend {
    fn from(value: Blend) -> Self {
        match value {
            Blend::None => Self::None,
            Blend::Premultiplied => Self::Premultiplied,
            Blend::Coverage => Self::Coverage,
        }
    }
}
