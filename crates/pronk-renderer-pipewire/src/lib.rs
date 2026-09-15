//! Registration of userspace-rendered outputs with private PipeWire transport.

mod output;
mod registration;
pub use output::{OutputEvent, OutputSession, PublishError};
pub use registration::Registration;

use std::io;

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
