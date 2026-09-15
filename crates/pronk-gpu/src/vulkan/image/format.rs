//! Packed native channels, independent of output transport and plane blending.

use ash::vk;

/// Normalized color channels and optional alpha in one packed pixel.
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
    /// Sixteen-bit RGB565: R in bits 11..15, G in 5..10 and B in 0..4.
    /// There is no stored alpha; native conversion supplies opaque alpha.
    Rgb565,
}

impl PackedFormat {
    /// Bytes occupied by one pixel before row or allocation padding.
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgb565 => 2,
            Self::Bgra8 | Self::Rgba8 | Self::Bgr10A2 | Self::Rgb10A2 => 4,
        }
    }

    pub(in crate::vulkan) fn native(self) -> vk::Format {
        match self {
            Self::Bgra8 => vk::Format::B8G8R8A8_UNORM,
            Self::Rgba8 => vk::Format::R8G8B8A8_UNORM,
            Self::Bgr10A2 => vk::Format::A2R10G10B10_UNORM_PACK32,
            Self::Rgb10A2 => vk::Format::A2B10G10R10_UNORM_PACK32,
            Self::Rgb565 => vk::Format::R5G6B5_UNORM_PACK16,
        }
    }
}
