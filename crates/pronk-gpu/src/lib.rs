//! GPU output ownership outside capture policy and media transport loops.

pub mod output_pool;

#[cfg(feature = "vulkan")]
pub mod vulkan;
