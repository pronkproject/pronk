//! Output-pool registration with one private PipeWire source.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};

use pronk_pipewire::{
    VideoBuffer, VideoBufferLayout, VideoBufferStorage, VideoPixelFormat, MAX_VIDEO_BUFFERS,
    MIN_VIDEO_BUFFERS,
};
use pronk_renderer_worker::OutputPool;

use crate::{invalid, OutputSession};

/// An output pool awaiting registration with one PipeWire generation.
pub struct Registration {
    pool: OutputPool,
    layout: VideoBufferLayout,
}

impl Registration {
    /// Validate the renderer layout against the current packed RGB transport.
    pub fn new(pool: OutputPool) -> io::Result<Self> {
        if !(MIN_VIDEO_BUFFERS..=MAX_VIDEO_BUFFERS).contains(&pool.len()) {
            return Err(invalid("renderer pool is outside PipeWire buffer limits"));
        }
        let image = pool.layout();
        if image.format != pronk_gpu::vulkan::PackedFormat::Bgra8 {
            return Err(invalid(
                "renderer output does not use the BGRx transport format",
            ));
        }
        let pitch = NonZeroU32::new(
            image
                .pitch
                .try_into()
                .map_err(|_| invalid("renderer output pitch is too large"))?,
        )
        .ok_or_else(|| invalid("renderer output has a zero pitch"))?;
        let size = NonZeroU64::new(image.allocation_size)
            .ok_or_else(|| invalid("renderer output has no storage"))?;
        let offset = u32::try_from(image.offset)
            .map_err(|_| invalid("renderer output offset is too large"))?;
        let layout = VideoBufferLayout {
            format: VideoPixelFormat::Xrgb8888,
            width: image.width,
            height: image.height,
            pitch,
            size,
            storage: VideoBufferStorage::DrmModifier {
                modifier: image.modifier,
                offset,
            },
        };
        layout.validate().map_err(io::Error::other)?;
        Ok(Self { pool, layout })
    }

    pub fn layout(&self) -> VideoBufferLayout {
        self.layout
    }

    /// Export every buffer without transferring the pool's ownership.
    pub fn export(&self) -> io::Result<Vec<VideoBuffer>> {
        (0..self.pool.len())
            .map(|slot| {
                Ok(VideoBuffer {
                    id: NonZeroU32::new(u32::try_from(slot + 1).expect("bounded output slot"))
                        .expect("one-based output slot"),
                    dma_buf: self.pool.export(slot)?,
                    layout: self.layout,
                    timelines: None,
                })
            })
            .collect()
    }

    /// Bind the pool to the identity returned by PipeWire startup.
    pub fn bind(self, identity: pronk_pipewire::VideoNodeIdentity) -> OutputSession {
        OutputSession::new(self.pool, self.layout, identity)
    }
}
