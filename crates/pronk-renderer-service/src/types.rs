use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

/// Storage policy for renderer-private scene images.
#[derive(Clone, Copy, Debug)]
pub struct PrivatePoolConfig {
    pub modifier: u64,
    pub frame_capacity: NonZeroUsize,
    pub source_capacity: NonZeroUsize,
}

/// Allocation and scheduling policy for one renderer generation.
#[derive(Clone, Copy, Debug)]
pub struct RendererStreamConfig {
    pub output_width: NonZeroU32,
    pub output_height: NonZeroU32,
    pub source_interval: Duration,
    pub private_pool: PrivatePoolConfig,
}

/// Observable lifetime of one renderer stream task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererStreamState {
    Prepared,
    Active,
    Stopped,
    Failed(String),
}
