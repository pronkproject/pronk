//! Packed native channels, independent of output transport and plane blending.

use ash::vk;

/// Four normalized eight-bit channels in their memory byte order.
///
/// The format describes storage, not whether alpha is ignored, premultiplied
/// or blended. Encoded RGB values are preserved without sRGB conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedFormat {
    Bgra8,
}

impl PackedFormat {
    pub(in crate::vulkan) fn native(self) -> vk::Format {
        match self {
            Self::Bgra8 => vk::Format::B8G8R8A8_UNORM,
        }
    }
}
