//! Packed RGB formats for the initial reference-rendering profile.

/// Four-byte, little-endian DRM pixel encodings, independent of blend or color space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackedRgbFormat {
    /// Bytes B, G, R, ignored padding.
    Xrgb8888,
    /// Bytes B, G, R, A.
    Argb8888,
    /// Bytes R, G, B, ignored padding.
    Xbgr8888,
    /// Bytes R, G, B, A.
    Abgr8888,
}
