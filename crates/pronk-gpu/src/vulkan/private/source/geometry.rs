//! Checked rectangles for source reads into independent private images.

use std::io;

use ash::vk;
use drm_display_executor::scene::geometry::{Extent, SourceRect};

pub(super) struct Blit {
    pub(super) region: vk::ImageBlit,
    pub(super) fills_destination: bool,
}

impl Blit {
    pub(super) fn new(
        source: Extent,
        destination: Extent,
        crop: SourceRect,
        position: [u32; 2],
        extent: Extent,
    ) -> io::Result<Self> {
        if crop.image() != source {
            return Err(invalid("source crop describes different image dimensions"));
        }
        let target = SourceRect::new(destination, position, extent).map_err(invalid)?;
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        Ok(Self {
            region: vk::ImageBlit::default()
                .src_subresource(layers)
                .src_offsets(offsets(crop)?)
                .dst_subresource(layers)
                .dst_offsets(offsets(target)?),
            fills_destination: position == [0, 0] && extent == destination,
        })
    }
}

fn offsets(rectangle: SourceRect) -> io::Result<[vk::Offset3D; 2]> {
    let [x, y] = rectangle.origin();
    // The checked rectangle keeps the sums within its u32-sized image. Vulkan
    // blit coordinates additionally need signed representations for both edges.
    Ok([
        vk::Offset3D {
            x: x.try_into().map_err(invalid)?,
            y: y.try_into().map_err(invalid)?,
            z: 0,
        },
        vk::Offset3D {
            x: (x + rectangle.extent().width())
                .try_into()
                .map_err(invalid)?,
            y: (y + rectangle.extent().height())
                .try_into()
                .map_err(invalid)?,
            z: 1,
        },
    ])
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(width: u32, height: u32) -> Extent {
        Extent::new(width, height).unwrap()
    }

    #[test]
    fn cropped_and_scaled_rectangles_have_independent_edges() {
        let image = extent(29, 17);
        let crop = SourceRect::new(image, [3, 5], extent(11, 7)).unwrap();
        let blit = Blit::new(image, extent(41, 31), crop, [9, 1], extent(22, 21)).unwrap();
        assert_eq!(
            blit.region.src_offsets,
            [
                vk::Offset3D { x: 3, y: 5, z: 0 },
                vk::Offset3D { x: 14, y: 12, z: 1 },
            ]
        );
        assert_eq!(
            blit.region.dst_offsets,
            [
                vk::Offset3D { x: 9, y: 1, z: 0 },
                vk::Offset3D { x: 31, y: 22, z: 1 },
            ]
        );
        assert!(!blit.fills_destination);
    }

    #[test]
    fn full_destination_needs_no_background_clear() {
        let image = extent(29, 17);
        let crop = SourceRect::new(image, [3, 5], extent(11, 7)).unwrap();
        let output = extent(22, 21);
        assert!(
            Blit::new(image, output, crop, [0, 0], output)
                .unwrap()
                .fills_destination
        );
    }

    #[test]
    fn reject_wrong_source_and_out_of_bounds_destination() {
        let image = extent(29, 17);
        let crop = SourceRect::new(image, [3, 5], extent(11, 7)).unwrap();
        for (source, position, size) in [
            (extent(30, 17), [0, 0], extent(29, 17)),
            (image, [1, 0], extent(29, 17)),
            (image, [0, 1], extent(29, 17)),
            (image, [u32::MAX, 0], extent(1, 1)),
        ] {
            assert_eq!(
                Blit::new(source, image, crop, position, size)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn reject_unsigned_edges_that_vulkan_cannot_represent() {
        let image = extent(u32::MAX, u32::MAX);
        let small = extent(1, 1);
        let crop = SourceRect::new(image, [i32::MAX as u32, 0], small).unwrap();
        assert!(Blit::new(image, small, crop, [0, 0], small).is_err());
        let crop = SourceRect::new(small, [0, 0], small).unwrap();
        assert!(Blit::new(small, image, crop, [0, i32::MAX as u32], small).is_err());
        let limit = extent(i32::MAX as u32, i32::MAX as u32);
        let crop = SourceRect::new(limit, [0, 0], limit).unwrap();
        assert!(Blit::new(limit, limit, crop, [0, 0], limit).is_ok());
    }
}
