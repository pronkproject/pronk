//! Registration of userspace-rendered outputs with private PipeWire transport.

mod output;
mod registration;
mod video;
pub use output::{OutputEvent, OutputFrame, OutputSession, PublishError};
pub use registration::Registration;
pub use video::{FramePublishError, StoppedVideo, Video, VideoEvent};

use std::io;

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
