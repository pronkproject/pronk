//! Packed native channels, independent of output transport and plane blending.

use ash::vk;

/// Normalized color and alpha channels in one 32-bit packed pixel.
///
/// The format describes storage, not whether alpha is ignored, premultiplied
/// or blended. Encoded RGB values are preserved without sRGB conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedFormat {
    /// Bytes B, G, R, A, with eight bits per channel.
    Bgra8,
    /// Bytes R, G, B, A, with eight bits per channel.
    Rgba8,
    /// B, G and R in bits 0..9, 10..19 and 20..29; A in bits 30..31.
    Bgr10A2,
    /// R, G and B in bits 0..9, 10..19 and 20..29; A in bits 30..31.
    Rgb10A2,
}

impl PackedFormat {
    pub(in crate::vulkan) fn native(self) -> vk::Format {
        match self {
            Self::Bgra8 => vk::Format::B8G8R8A8_UNORM,
            Self::Rgba8 => vk::Format::R8G8B8A8_UNORM,
            Self::Bgr10A2 => vk::Format::A2R10G10B10_UNORM_PACK32,
            Self::Rgb10A2 => vk::Format::A2B10G10R10_UNORM_PACK32,
        }
    }
}
