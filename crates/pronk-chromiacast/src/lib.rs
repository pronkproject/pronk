//! Google Cast device backend for Pronk.
//!
//! Process transport, discovery, and the private D-Bus adapter remain separate
//! so network discovery cannot begin before backend registration succeeds.

mod audio_sender_actor;
mod backend;
mod cast_transport;
mod device;
mod discovery;
mod feedback;
mod media;
mod process;
mod sender_actor;
mod session;
mod transport;

pub use process::{run, StartupConfiguration};
