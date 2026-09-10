//! Orthogonal pixel transforms, without filtering or source access.

use super::geometry::Extent;

/// Counter-clockwise rotation in image coordinates, matching DRM convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    #[default]
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
}

/// Reflect source axes first, then rotate counter-clockwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transform {
    pub rotation: Rotation,
    pub reflect_x: bool,
    pub reflect_y: bool,
}

impl Transform {
    /// Dimensions of the transformed crop, with no scaling.
    pub fn extent(self, source: Extent) -> Extent {
        match self.rotation {
            Rotation::Rotate0 | Rotation::Rotate180 => source,
            Rotation::Rotate90 | Rotation::Rotate270 => {
                Extent::new(source.height(), source.width()).expect("nonzero source dimensions")
            }
        }
    }

    /// Map a pixel in the transformed crop back to its original crop coordinates.
    ///
    /// Returns `None` outside the transformed extent. Coordinates are local to
    /// the crop: the caller adds its validated source-image origin afterward.
    pub fn source_at(self, source: Extent, pixel: [u32; 2]) -> Option<[u32; 2]> {
        let output = self.extent(source);
        let [x, y] = pixel;
        if x >= output.width() || y >= output.height() {
            return None;
        }
        let width = source.width();
        let height = source.height();
        let [mut x, mut y] = match self.rotation {
            Rotation::Rotate0 => [x, y],
            Rotation::Rotate90 => [width - 1 - y, x],
            Rotation::Rotate180 => [width - 1 - x, height - 1 - y],
            Rotation::Rotate270 => [y, height - 1 - x],
        };
        if self.reflect_x {
            x = width - 1 - x;
        }
        if self.reflect_y {
            y = height - 1 - y;
        }
        Some([x, y])
    }
}
