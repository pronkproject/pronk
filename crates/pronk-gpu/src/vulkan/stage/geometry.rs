//! Checked adaptation from shared pixel geometry to native copy commands.

use std::io;

use ash::vk;
use drm_display_executor::scene::geometry::{Extent, SourceRect};

use crate::vulkan::ImageLayout;

pub(super) struct Copy {
    pub(super) region: vk::ImageCopy,
    pub(super) background: Option<[u8; 3]>,
}

impl Copy {
    pub(super) fn whole(source: ImageLayout, destination: ImageLayout) -> io::Result<Self> {
        if source.width != destination.width || source.height != destination.height {
            return Err(invalid("whole-image staging needs matching extents"));
        }
        Ok(Self {
            region: region(
                [0, 0],
                [0, 0],
                Extent::new(source.width.get(), source.height.get()).map_err(invalid)?,
            )?,
            background: None,
        })
    }

    pub(super) fn placed(
        image: ImageLayout,
        output: ImageLayout,
        source: SourceRect,
        destination: [i32; 2],
        background: [u8; 3],
    ) -> io::Result<Self> {
        if source.image().width() != image.width.get()
            || source.image().height() != image.height.get()
        {
            return Err(invalid("source crop describes different image dimensions"));
        }
        let output = Extent::new(output.width.get(), output.height.get()).map_err(invalid)?;
        let visible = source
            .clip_to(destination, output)
            .ok_or_else(|| invalid("source placement has no visible pixels"))?;
        Ok(Self {
            region: region(visible.source(), visible.destination(), visible.extent())?,
            background: Some(background),
        })
    }
}

fn region(source: [u32; 2], destination: [u32; 2], extent: Extent) -> io::Result<vk::ImageCopy> {
    let layers = vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1);
    Ok(vk::ImageCopy::default()
        .src_subresource(layers)
        .src_offset(offset(source)?)
        .dst_subresource(layers)
        .dst_offset(offset(destination)?)
        .extent(vk::Extent3D {
            width: extent.width(),
            height: extent.height(),
            depth: 1,
        }))
}

fn offset(value: [u32; 2]) -> io::Result<vk::Offset3D> {
    Ok(vk::Offset3D {
        x: i32::try_from(value[0]).map_err(invalid)?,
        y: i32::try_from(value[1]).map_err(invalid)?,
        z: 0,
    })
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}
