//! Core-side owner for one allow-listed device backend connection.

mod connection;
mod discovery;
mod endpoint;
mod inventory;
mod message_credentials;
mod registry;
mod session;
mod session_monitor;
mod supervisor;
mod systemd;

pub use connection::{
    BackendConnectError, BackendConnection, BackendConnectionError, BackendInstanceControlError,
    BackendRegistrationValidator, ExactRegistrationValidator, RegistrationValidationError,
};
pub use discovery::{DiscoveryError, DiscoveryNotification};
pub use endpoint::{BackendEndpoint, EndpointError};
pub use inventory::{DeviceInventorySnapshot, InventoryError};
pub use registry::{
    BackendRegistry, BackendRegistryError, InstalledBackend, MAX_INSTALLED_BACKENDS,
};
pub use session::{BackendSessionError, BackendSessionHandle, BackendSessionRequest};
pub use session_monitor::{BackendSessionEvent, BackendSessionMonitor};
pub use supervisor::{
    BackendDisconnectReason, BackendHandle, BackendReconnectPolicy, BackendRetryError,
    BackendShutdownReport, BackendSupervisor, BackendSupervisorError, BackendSupervisorEvent,
};
pub use systemd::SystemdRegistrationValidator;
