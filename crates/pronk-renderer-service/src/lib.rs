//! Lifetime supervision for a userspace renderer selected by KMS constraints.

mod active;
mod native_task;
mod renderer;
mod task;
mod types;
pub use renderer::{ActiveRendererStream, RendererStream, RendererStreamError};
pub use types::{PrivatePoolConfig, RendererStreamConfig, RendererStreamState};
