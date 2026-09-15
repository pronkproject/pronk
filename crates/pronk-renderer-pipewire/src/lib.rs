//! Registration of userspace-rendered outputs with private PipeWire transport.

mod active;
mod native_task;
mod output;
mod registration;
mod renderer;
mod task;
mod types;
mod video;
pub use active::run_complete_scenes;
pub use output::{OutputEvent, OutputFrame, OutputSession, PublishError};
pub use registration::Registration;
pub use renderer::{ActiveRendererStream, RendererStream, RendererStreamError};
pub use types::{RendererStreamConfig, RendererStreamState};
pub use video::{FramePublishError, StoppedVideo, Video, VideoEvent};

use std::io;

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
