use std::num::NonZeroUsize;

use pronk_pipewire::VideoSourceConfig;

/// Allocation and transport policy for one renderer generation.
#[derive(Clone, Debug)]
pub struct RendererStreamConfig {
    pub pipewire: VideoSourceConfig,
    pub output_modifier: u64,
    pub private_capacity: NonZeroUsize,
    pub output_capacity: NonZeroUsize,
}

/// Observable lifetime of one renderer stream task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererStreamState {
    Prepared,
    Active,
    Stopped,
    Failed(String),
}
