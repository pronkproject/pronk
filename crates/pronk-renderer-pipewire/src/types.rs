use std::num::{NonZeroU32, NonZeroUsize};

use pronk_pipewire::VideoSourceConfig;

/// Storage policy for renderer-private scene images.
#[derive(Clone, Copy, Debug)]
pub struct PrivatePoolConfig {
    pub modifier: u64,
    pub frame_capacity: NonZeroUsize,
    pub source_capacity: NonZeroUsize,
}

/// Storage policy for images exported to the media pipeline.
#[derive(Clone, Copy, Debug)]
pub struct OutputPoolConfig {
    pub modifier: u64,
    pub capacity: NonZeroUsize,
}

/// Allocation and transport policy for one renderer generation.
#[derive(Clone, Debug)]
pub struct RendererStreamConfig {
    pub output_width: NonZeroU32,
    pub output_height: NonZeroU32,
    pub pipewire: VideoSourceConfig,
    pub private_pool: PrivatePoolConfig,
    pub output_pool: OutputPoolConfig,
}

/// Observable lifetime of one renderer stream task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererStreamState {
    Prepared,
    Active,
    Stopped,
    Failed(String),
}
