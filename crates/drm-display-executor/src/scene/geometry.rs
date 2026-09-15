//! Checked integral source crops and destination geometry.

use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryError {
    EmptyExtent,
    SourceOutsideImage,
    FractionalSource,
}

impl std::fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyExtent => "image extent must be nonzero",
            Self::SourceOutsideImage => "source rectangle exceeds its image",
            Self::FractionalSource => {
                "integral rendering does not accept fractional source coordinates"
            }
        })
    }
}

impl std::error::Error for GeometryError {}

/// Nonempty pixel dimensions; native format and device limits remain separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl Extent {
    pub fn new(width: u32, height: u32) -> Result<Self, GeometryError> {
        Ok(Self {
            width: NonZeroU32::new(width).ok_or(GeometryError::EmptyExtent)?,
            height: NonZeroU32::new(height).ok_or(GeometryError::EmptyExtent)?,
        })
    }

    pub fn width(self) -> u32 {
        self.width.get()
    }

    pub fn height(self) -> u32 {
        self.height.get()
    }
}

/// A nonempty destination rectangle before clipping to an output.
///
/// Position may be negative or wholly outside the output. Dimensions describe
/// the scaled, transformed crop; they do not describe an allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestinationRect {
    pub position: [i32; 2],
    pub extent: Extent,
}

/// An integral crop checked against the dimensions of its source image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRect {
    image: Extent,
    origin: [u32; 2],
    extent: Extent,
}

impl SourceRect {
    /// Decode DRM-style unsigned 16.16 source x, y, width and height exactly.
    ///
    /// Fractional origins and extents are unsupported, not rounded or truncated.
    /// The resulting crop is checked against the actual source image. Output
    /// scaling, rotation, pixel format and source authority remain separate
    /// validation obligations; accepting a crop does not accept an entire plane.
    pub fn from_fixed_16_16(image: Extent, source: [u32; 4]) -> Result<Self, GeometryError> {
        if source.iter().any(|value| value & 0xffff != 0) {
            return Err(GeometryError::FractionalSource);
        }
        let [x, y, width, height] = source.map(|value| value >> 16);
        Self::new(image, [x, y], Extent::new(width, height)?)
    }

    pub fn new(image: Extent, origin: [u32; 2], extent: Extent) -> Result<Self, GeometryError> {
        if u64::from(origin[0]) + u64::from(extent.width()) > u64::from(image.width())
            || u64::from(origin[1]) + u64::from(extent.height()) > u64::from(image.height())
        {
            return Err(GeometryError::SourceOutsideImage);
        }
        Ok(Self {
            image,
            origin,
            extent,
        })
    }

    pub fn image(self) -> Extent {
        self.image
    }

    pub fn origin(self) -> [u32; 2] {
        self.origin
    }

    pub fn extent(self) -> Extent {
        self.extent
    }

    /// Clip an unscaled, unrotated placement to the output's visible pixels.
    ///
    /// Negative placement advances the source origin by exactly the clipped
    /// distance. A fully offscreen placement returns no copy, not invalid input.
    pub fn clip_to(self, destination: [i32; 2], output: Extent) -> Option<CopyRegion> {
        let x = i64::from(destination[0]);
        let y = i64::from(destination[1]);
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + i64::from(self.extent.width())).min(i64::from(output.width()));
        let bottom = (y + i64::from(self.extent.height())).min(i64::from(output.height()));
        if right <= left || bottom <= top {
            return None;
        }
        // Intersections lie inside u32-sized images. Advancing by the clipped
        // distance stays inside the source rectangle validated at construction.
        Some(CopyRegion {
            source: [
                (i64::from(self.origin[0]) + left - x) as u32,
                (i64::from(self.origin[1]) + top - y) as u32,
            ],
            destination: [left as u32, top as u32],
            extent: Extent::new((right - left) as u32, (bottom - top) as u32).ok()?,
        })
    }
}

/// A nonempty one-to-one pixel copy, bounded by the validated input and output.
///
/// This is geometry only: it grants no access to either allocation and says
/// nothing about producer completion, pixel formats or color processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyRegion {
    source: [u32; 2],
    destination: [u32; 2],
    extent: Extent,
}

impl CopyRegion {
    pub fn source(self) -> [u32; 2] {
        self.source
    }

    pub fn destination(self) -> [u32; 2] {
        self.destination
    }

    pub fn extent(self) -> Extent {
        self.extent
    }
}
