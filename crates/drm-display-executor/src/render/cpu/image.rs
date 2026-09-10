//! Checked linear views; no native mapping, cache maintenance or device access.

use crate::scene::{format::PackedRgbFormat, geometry::Extent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    ShortStride,
    AddressOverflow,
    ShortBuffer,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ShortStride => "stride does not cover a pixel row",
            Self::AddressOverflow => "linear image address exceeds the host address range",
            Self::ShortBuffer => "buffer does not cover the declared linear image",
        })
    }
}

impl std::error::Error for ImageError {}

/// Checked four-byte pixel layout, allowing leading and inter-row padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinearLayout {
    extent: Extent,
    format: PackedRgbFormat,
    offset: usize,
    stride: usize,
    row_bytes: usize,
    required: usize,
}

impl LinearLayout {
    pub fn new(
        extent: Extent,
        format: PackedRgbFormat,
        offset: usize,
        stride: usize,
    ) -> Result<Self, ImageError> {
        let row_bytes = usize::try_from(extent.width())
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(ImageError::AddressOverflow)?;
        if stride < row_bytes {
            return Err(ImageError::ShortStride);
        }
        let required = usize::try_from(extent.height() - 1)
            .ok()
            .and_then(|rows| rows.checked_mul(stride))
            .and_then(|bytes| bytes.checked_add(offset))
            .and_then(|bytes| bytes.checked_add(row_bytes))
            .ok_or(ImageError::AddressOverflow)?;
        Ok(Self {
            extent,
            format,
            offset,
            stride,
            row_bytes,
            required,
        })
    }

    pub fn extent(self) -> Extent {
        self.extent
    }

    pub fn format(self) -> PackedRgbFormat {
        self.format
    }

    /// Minimum storage, excluding padding after the last pixel row.
    pub fn required_bytes(self) -> usize {
        self.required
    }

    fn row(self, y: u32) -> Option<std::ops::Range<usize>> {
        if y >= self.extent.height() {
            return None;
        }
        let start = self.offset + y as usize * self.stride;
        Some(start..start + self.row_bytes)
    }
}

/// Shared initialized pixels. The caller supplies any external synchronization.
#[derive(Clone, Copy)]
pub struct Image<'a> {
    bytes: &'a [u8],
    layout: LinearLayout,
}

impl<'a> Image<'a> {
    pub fn new(bytes: &'a [u8], layout: LinearLayout) -> Result<Self, ImageError> {
        if bytes.len() < layout.required {
            return Err(ImageError::ShortBuffer);
        }
        Ok(Self { bytes, layout })
    }

    pub fn layout(self) -> LinearLayout {
        self.layout
    }

    /// Visible pixels only; leading, inter-row and trailing padding is excluded.
    pub fn row(self, y: u32) -> Option<&'a [u8]> {
        self.bytes.get(self.layout.row(y)?)
    }
}

/// Exclusive writable pixels, borrowed for the lifetime of the view.
///
/// ```compile_fail
/// use drm_display_executor::render::cpu::image::ImageMut;
/// fn conflicting_rows(image: &mut ImageMut<'_>) {
///     let first = image.row(0).unwrap();
///     let second = image.row(0).unwrap();
///     first[0] = second[0];
/// }
/// ```
pub struct ImageMut<'a> {
    bytes: &'a mut [u8],
    layout: LinearLayout,
}

impl<'a> ImageMut<'a> {
    pub fn new(bytes: &'a mut [u8], layout: LinearLayout) -> Result<Self, ImageError> {
        if bytes.len() < layout.required {
            return Err(ImageError::ShortBuffer);
        }
        Ok(Self { bytes, layout })
    }

    pub fn layout(&self) -> LinearLayout {
        self.layout
    }

    /// Mutable visible pixels, tied to the exclusive borrow of this view.
    pub fn row(&mut self, y: u32) -> Option<&mut [u8]> {
        self.bytes.get_mut(self.layout.row(y)?)
    }
}
