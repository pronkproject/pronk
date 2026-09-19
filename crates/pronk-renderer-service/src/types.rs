use pronk_gpu::vulkan::PackedFormat;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

/// Storage policy for renderer-private scene images.
#[derive(Clone, Copy, Debug)]
pub struct PrivatePoolConfig {
    /// None chooses an exportable layout on the selected GPU.
    pub modifier: Option<u64>,
    /// Upper bound; the scene budget can select fewer at larger modes.
    pub frame_capacity: NonZeroUsize,
    /// Upper bound; the scene budget can select fewer at larger modes.
    pub source_capacity: NonZeroUsize,
}

/// Allocation and scheduling policy for one renderer generation.
#[derive(Clone, Copy, Debug)]
pub struct RendererStreamConfig {
    pub output_width: NonZeroU32,
    pub output_height: NonZeroU32,
    pub output_format: PackedFormat,
    pub source_interval: Duration,
    pub private_pool: PrivatePoolConfig,
}

/// Observable lifetime of one renderer stream task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererStreamState {
    Starting,
    Running,
    Stopped,
    Failed(String),
}
