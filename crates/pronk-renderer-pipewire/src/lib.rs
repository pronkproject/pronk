//! Registration of userspace-rendered outputs with private PipeWire transport.

mod output;
pub use output::{OutputEvent, PublishError, TransportOutput};

use std::io;
use std::num::{NonZeroU32, NonZeroU64};

use pronk_pipewire::{
    VideoBuffer, VideoBufferLayout, VideoBufferStorage, VideoPixelFormat, MAX_VIDEO_BUFFERS,
    MIN_VIDEO_BUFFERS,
};
use pronk_renderer_worker::{OutputPool, OutputScope};

/// Immutable transport description for one renderer output pool.
pub struct Registration {
    scope: OutputScope,
    layout: VideoBufferLayout,
    count: usize,
}

impl Registration {
    /// Validate the renderer layout against the current packed RGB transport.
    pub fn new(pool: &OutputPool) -> io::Result<Self> {
        if !(MIN_VIDEO_BUFFERS..=MAX_VIDEO_BUFFERS).contains(&pool.len()) {
            return Err(invalid("renderer pool is outside PipeWire buffer limits"));
        }
        let image = pool.layout();
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
        Ok(Self {
            scope: pool.scope(),
            layout,
            count: pool.len(),
        })
    }

    pub fn layout(&self) -> VideoBufferLayout {
        self.layout
    }

    /// Export every buffer without granting access to unpublished pixels.
    pub fn export(&self, pool: &OutputPool) -> io::Result<Vec<VideoBuffer>> {
        if pool.scope() != self.scope || pool.len() != self.count {
            return Err(invalid("renderer registration belongs to another pool"));
        }
        (0..self.count)
            .map(|slot| {
                Ok(VideoBuffer {
                    id: NonZeroU32::new(u32::try_from(slot + 1).expect("bounded output slot"))
                        .expect("one-based output slot"),
                    dma_buf: pool.export(slot)?,
                    layout: self.layout,
                    timelines: None,
                })
            })
            .collect()
    }

    /// Bind publication tracking to the identity returned by PipeWire startup.
    pub fn bind(self, identity: pronk_pipewire::VideoNodeIdentity) -> TransportOutput {
        TransportOutput::new(self.scope, self.layout, self.count, identity)
    }
}

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
