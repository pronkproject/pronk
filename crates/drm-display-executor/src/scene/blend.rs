//! Plane blending policy, independent of pixel encoding and native storage.

/// Interpretation of source pixel alpha in the DRM plane blend equations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelBlend {
    /// Ignore pixel alpha, including alpha carried by the image format.
    None,
    /// Source color components already include pixel alpha.
    Premultiplied,
    /// Multiply source color components by pixel alpha while blending.
    Coverage,
}

/// Pixel interpretation and normalized 16-bit plane-wide opacity.
///
/// Zero plane alpha contributes no source color; `u16::MAX` is fully opaque.
/// Formats without pixel alpha supply fully opaque pixel alpha in every mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blend {
    pub pixel: PixelBlend,
    pub plane_alpha: u16,
}

impl Default for Blend {
    fn default() -> Self {
        Self {
            pixel: PixelBlend::Premultiplied,
            plane_alpha: u16::MAX,
        }
    }
}
