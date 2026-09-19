//! Capture-file ownership and actor creation across media generations.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use drm_capture::{Client, Description, RequestedLayout};

use crate::names::Names;
use crate::{invalid, native, worker, Actor, Buffer, Config};

/// Owns the stream and destination namespace for one issued capture file.
///
/// Create exactly one session for a file description, not one per duplicated
/// descriptor or media generation. Other clients may observe the file, but
/// must not allocate stream or destination names in the same namespace.
/// Actors retain the file independently. Dropping the session does not stop
/// those actors or establish ended destination access.
pub struct Session<F = OwnedFd> {
    client: Arc<Client<F>>,
    names: Names,
}

impl<F: AsFd + Send + Sync + 'static> Session<F> {
    pub fn new(client: Client<F>) -> Self {
        Self {
            client: Arc::new(client),
            names: Names::default(),
        }
    }

    pub fn describe(&self) -> io::Result<drm_capture::Description> {
        self.client.describe()
    }

    pub fn describe_layout(&self, layout: RequestedLayout) -> io::Result<Description> {
        self.client.describe_layout(layout)
    }

    /// Open a new stream for the currently active output using fresh identities.
    ///
    /// Configuration changes require a new actor and fresh storage, not a new
    /// authorization. Failed setup also consumes its reserved names. No grant,
    /// primary DRM file or issuer identity reaches consumers.
    pub fn spawn(
        &mut self,
        buffers: Vec<Buffer>,
        config: Config,
    ) -> io::Result<Actor<Arc<Client<F>>>> {
        self.spawn_expected(buffers, config, None)
    }

    /// Open a stream only if the named offer remains current.
    ///
    /// Stream opening checks the named offer again before any destinations are
    /// registered. The retained description supplies the exact buffer layout.
    pub fn spawn_for_offer(
        &mut self,
        buffers: Vec<Buffer>,
        config: Config,
        description: Description,
    ) -> io::Result<Actor<Arc<Client<F>>>> {
        self.spawn_expected(buffers, config, Some(description))
    }

    fn spawn_expected(
        &mut self,
        buffers: Vec<Buffer>,
        config: Config,
        description: Option<Description>,
    ) -> io::Result<Actor<Arc<Client<F>>>> {
        config.validate(buffers.len())?;
        // Require a runtime context before opening kernel state. Its timers
        // must also be enabled for completion polling and shutdown deadlines.
        tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("capture actor requires a Tokio runtime"))?;
        let registration = self.names.reserve(buffers.len())?;
        let client = Client::from_owner(Arc::clone(&self.client))?;
        let (backend, layout) =
            native::Native::open(client, &buffers, config, registration, description)?;
        Ok(worker::spawn(backend, buffers, layout, config))
    }
}
