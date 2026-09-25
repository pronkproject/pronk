//! Public D-Bus adapter for Device inventory and cast-display setup.

mod interfaces;
mod public_state;
mod signals;

pub use signals::{emit_inventory_events, register_manager, serve_lifecycle_events};

#[cfg(test)]
use crate::display::CastDisplayId;
#[cfg(test)]
use crate::manager::{InventoryEvent, LifecycleEvent};
#[cfg(test)]
use interfaces::{display_path, CastDisplayInterface, ManagerInterface};
#[cfg(test)]
use pronk_dbus::MANAGER_PATH;
#[cfg(test)]
use pronk_dbus::{
    ApiVersion, CastDisplayInfo, CastDisplaySnapshot, CastDisplayState, DeviceInfo, DeviceSnapshot,
    DisplayAttachmentState, DisplayIdentitySource, DisplayRouteState, PnpResolutionSource,
};
#[cfg(test)]
use public_state::{public_display, public_display_state, public_media_session_state};
#[cfg(test)]
use tokio::sync::mpsc;

#[cfg(test)]
mod tests;
