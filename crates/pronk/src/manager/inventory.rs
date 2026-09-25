//! Aggregate backend inventory and public revision ordering.

use super::{InventoryEvent, ResolveDeviceError};
use pronk_backend_host::{BackendSupervisorEvent, DeviceInventorySnapshot};
use pronk_backend_protocol::DeviceAvailability as BackendAvailability;
use pronk_backend_protocol::Validate;
use pronk_dbus::{
    DeviceAvailability, DeviceInfo, DeviceSelection, DeviceSnapshot, DiscoveryMetadataEntry,
    MAX_PUBLIC_DEVICES,
};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Default)]
pub(super) struct AggregateInventory {
    inventory_revision: u64,
    backends: BTreeMap<String, BackendInventory>,
}

#[derive(Debug, Default)]
struct BackendInventory {
    connection_generation: u64,
    discovery_generation: Option<u64>,
    devices: BTreeMap<String, DeviceInfo>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ApplySupervisorOutcome {
    Changed(Vec<InventoryEvent>),
    IgnoredStale,
}

impl AggregateInventory {
    pub(super) fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            inventory_revision: self.inventory_revision,
            devices: self
                .backends
                .values()
                .flat_map(|backend| backend.devices.values().cloned())
                .collect(),
        }
    }

    pub(super) fn configured_device(&self, previous: &DeviceInfo) -> DeviceInfo {
        if let Some(current) = self
            .backends
            .get(&previous.backend_id)
            .and_then(|backend| backend.devices.get(&previous.device_id))
        {
            return current.clone();
        }

        // Configured displays outlive passive discovery records. Preserve the
        // last bounded identity, but make absence explicit and advance its
        // token to the inventory revision that already observed the removal.
        let mut unavailable = previous.clone();
        unavailable.availability = DeviceAvailability::Unavailable;
        unavailable.device_revision = self.inventory_revision.max(previous.device_revision);
        unavailable
    }

    pub(super) fn resolve_device(
        &self,
        selection: &DeviceSelection,
    ) -> Result<DeviceInfo, ResolveDeviceError> {
        selection
            .validate()
            .map_err(|error| ResolveDeviceError::InvalidSelection(error.to_string()))?;
        let device = self
            .backends
            .get(&selection.backend_id)
            .and_then(|backend| backend.devices.get(&selection.device_id))
            .ok_or_else(|| ResolveDeviceError::NotFound {
                backend_id: selection.backend_id.clone(),
                device_id: selection.device_id.clone(),
            })?;
        if device.connection_generation != selection.connection_generation
            || device.discovery_generation != selection.discovery_generation
            || device.device_revision != selection.device_revision
        {
            return Err(ResolveDeviceError::StaleSelection {
                backend_id: selection.backend_id.clone(),
                device_id: selection.device_id.clone(),
            });
        }
        if device.availability != DeviceAvailability::Available {
            return Err(ResolveDeviceError::Unavailable {
                backend_id: selection.backend_id.clone(),
                device_id: selection.device_id.clone(),
                availability: device.availability,
            });
        }
        Ok(device.clone())
    }

    pub(super) fn apply_supervisor_event(
        &mut self,
        backend_id: &str,
        event: &BackendSupervisorEvent,
    ) -> Result<ApplySupervisorOutcome, AggregateError> {
        match event {
            BackendSupervisorEvent::Connecting {
                connection_generation,
            }
            | BackendSupervisorEvent::ConnectionFailed {
                connection_generation,
                ..
            } => self.advance_connection(backend_id, *connection_generation),
            BackendSupervisorEvent::Connected {
                connection_generation,
                inventory,
                ..
            } => self.replace_backend(backend_id, *connection_generation, inventory),
            BackendSupervisorEvent::InventoryChanged {
                connection_generation,
                inventory,
            }
            | BackendSupervisorEvent::InventoryResynchronized {
                connection_generation,
                inventory,
                ..
            }
            | BackendSupervisorEvent::Disconnected {
                connection_generation,
                unavailable_inventory: inventory,
                ..
            } => {
                let current = self
                    .backends
                    .get(backend_id)
                    .map_or(0, |backend| backend.connection_generation);
                if *connection_generation < current {
                    return Ok(ApplySupervisorOutcome::IgnoredStale);
                }
                if *connection_generation > current {
                    return Err(AggregateError::UnexpectedConnectionGeneration {
                        backend_id: backend_id.into(),
                        expected: current,
                        actual: *connection_generation,
                    });
                }
                self.replace_backend(backend_id, *connection_generation, inventory)
            }
            BackendSupervisorEvent::Stopped {
                last_connection_generation,
            } => {
                if let Some(last) = last_connection_generation {
                    let current = self
                        .backends
                        .get(backend_id)
                        .map_or(0, |backend| backend.connection_generation);
                    if *last < current {
                        return Ok(ApplySupervisorOutcome::IgnoredStale);
                    }
                }
                self.mark_backend_unavailable(backend_id)
                    .map(ApplySupervisorOutcome::Changed)
            }
            BackendSupervisorEvent::ReconnectScheduled { .. }
            | BackendSupervisorEvent::ReconnectExhausted { .. } => {
                Ok(ApplySupervisorOutcome::Changed(Vec::new()))
            }
        }
    }

    fn advance_connection(
        &mut self,
        backend_id: &str,
        connection_generation: u64,
    ) -> Result<ApplySupervisorOutcome, AggregateError> {
        let current = self
            .backends
            .get(backend_id)
            .map_or(0, |backend| backend.connection_generation);
        if connection_generation <= current {
            return Ok(ApplySupervisorOutcome::IgnoredStale);
        }
        let changes = self.mark_backend_unavailable(backend_id)?;
        let backend = self.backends.entry(backend_id.into()).or_default();
        backend.connection_generation = connection_generation;
        backend.discovery_generation = None;
        Ok(ApplySupervisorOutcome::Changed(changes))
    }

    fn replace_backend(
        &mut self,
        backend_id: &str,
        connection_generation: u64,
        snapshot: &DeviceInventorySnapshot,
    ) -> Result<ApplySupervisorOutcome, AggregateError> {
        let current = self
            .backends
            .get(backend_id)
            .map_or(0, |backend| backend.connection_generation);
        if connection_generation < current {
            return Ok(ApplySupervisorOutcome::IgnoredStale);
        }
        if connection_generation == 0
            || snapshot.discovery_generation == 0
            || snapshot.revision == 0
        {
            return Err(AggregateError::ZeroGeneration);
        }

        let old_devices = self
            .backends
            .get(backend_id)
            .map_or_else(BTreeMap::new, |backend| backend.devices.clone());
        let mut new_devices = BTreeMap::new();
        for device in &snapshot.devices {
            device
                .validate()
                .map_err(|error| AggregateError::InvalidDevice(error.to_string()))?;
            if device.backend_id != backend_id {
                return Err(AggregateError::WrongBackendId {
                    expected: backend_id.into(),
                    actual: device.backend_id.clone(),
                });
            }
            let existing = old_devices.get(&device.device_id);
            let public = public_device(
                device,
                connection_generation,
                snapshot.discovery_generation,
                existing,
            );
            if new_devices
                .insert(device.device_id.clone(), public)
                .is_some()
            {
                return Err(AggregateError::DuplicateDevice {
                    backend_id: backend_id.into(),
                    device_id: device.device_id.clone(),
                });
            }
        }

        let old_total = self.device_count();
        let new_total = old_total - old_devices.len() + new_devices.len();
        if new_total > MAX_PUBLIC_DEVICES {
            return Err(AggregateError::TooManyDevices(new_total));
        }

        let changes = diff_devices(&old_devices, &new_devices);
        self.ensure_revision_capacity(changes.len())?;
        let backend = self.backends.entry(backend_id.into()).or_default();
        backend.connection_generation = connection_generation;
        backend.discovery_generation = Some(snapshot.discovery_generation);
        backend.devices = new_devices;
        Ok(ApplySupervisorOutcome::Changed(
            self.revision_events(backend_id, changes),
        ))
    }

    pub(super) fn mark_backend_unavailable(
        &mut self,
        backend_id: &str,
    ) -> Result<Vec<InventoryEvent>, AggregateError> {
        let Some(backend) = self.backends.get(backend_id) else {
            return Ok(Vec::new());
        };
        let mut new_devices = backend.devices.clone();
        for device in new_devices.values_mut() {
            if device.availability != DeviceAvailability::Unavailable {
                device.availability = DeviceAvailability::Unavailable;
                device.device_revision = 0;
            }
        }
        let changes = diff_devices(&backend.devices, &new_devices);
        self.ensure_revision_capacity(changes.len())?;
        self.backends
            .get_mut(backend_id)
            .expect("backend disappeared")
            .devices = new_devices;
        Ok(self.revision_events(backend_id, changes))
    }

    pub(super) fn mark_all_unavailable(&mut self) -> Result<Vec<InventoryEvent>, AggregateError> {
        let backend_ids: Vec<_> = self.backends.keys().cloned().collect();
        let required = self
            .backends
            .values()
            .flat_map(|backend| backend.devices.values())
            .filter(|device| device.availability != DeviceAvailability::Unavailable)
            .count();
        self.ensure_revision_capacity(required)?;
        let mut events = Vec::with_capacity(required);
        for backend_id in backend_ids {
            events.extend(self.mark_backend_unavailable(&backend_id)?);
        }
        Ok(events)
    }

    fn device_count(&self) -> usize {
        self.backends
            .values()
            .map(|backend| backend.devices.len())
            .sum()
    }

    fn ensure_revision_capacity(&self, changes: usize) -> Result<(), AggregateError> {
        self.inventory_revision
            .checked_add(changes as u64)
            .ok_or(AggregateError::RevisionExhausted)
            .map(|_| ())
    }

    fn revision_events(
        &mut self,
        backend_id: &str,
        changes: Vec<DeviceChange>,
    ) -> Vec<InventoryEvent> {
        let mut events = Vec::with_capacity(changes.len());
        for change in changes {
            self.inventory_revision = self
                .inventory_revision
                .checked_add(1)
                .expect("revision capacity checked");
            let event = match change {
                DeviceChange::Added(mut device) => {
                    device.device_revision = self.inventory_revision;
                    self.backends
                        .get_mut(backend_id)
                        .and_then(|backend| backend.devices.get_mut(&device.device_id))
                        .expect("added device disappeared")
                        .device_revision = device.device_revision;
                    InventoryEvent::DeviceAdded {
                        inventory_revision: self.inventory_revision,
                        device,
                    }
                }
                DeviceChange::Changed(mut device) => {
                    device.device_revision = self.inventory_revision;
                    self.backends
                        .get_mut(backend_id)
                        .and_then(|backend| backend.devices.get_mut(&device.device_id))
                        .expect("changed device disappeared")
                        .device_revision = device.device_revision;
                    InventoryEvent::DeviceChanged {
                        inventory_revision: self.inventory_revision,
                        device,
                    }
                }
                DeviceChange::Removed(device_id) => InventoryEvent::DeviceRemoved {
                    inventory_revision: self.inventory_revision,
                    backend_id: backend_id.into(),
                    device_id,
                },
            };
            events.push(event);
        }
        events
    }
}

#[derive(Debug)]
enum DeviceChange {
    Added(DeviceInfo),
    Changed(DeviceInfo),
    Removed(String),
}

fn diff_devices(
    old: &BTreeMap<String, DeviceInfo>,
    new: &BTreeMap<String, DeviceInfo>,
) -> Vec<DeviceChange> {
    let mut changes = Vec::new();
    for device_id in old.keys() {
        if !new.contains_key(device_id) {
            changes.push(DeviceChange::Removed(device_id.clone()));
        }
    }
    for (device_id, device) in new {
        match old.get(device_id) {
            None => changes.push(DeviceChange::Added(device.clone())),
            Some(previous) if previous != device => {
                changes.push(DeviceChange::Changed(device.clone()));
            }
            Some(_) => {}
        }
    }
    changes
}

fn public_device(
    device: &pronk_backend_protocol::DeviceInfo,
    connection_generation: u64,
    discovery_generation: u64,
    existing: Option<&DeviceInfo>,
) -> DeviceInfo {
    let availability = match device.availability {
        BackendAvailability::Available => DeviceAvailability::Available,
        BackendAvailability::Busy => DeviceAvailability::Busy,
        BackendAvailability::Unavailable => DeviceAvailability::Unavailable,
    };
    let metadata: Vec<_> = device
        .metadata
        .iter()
        .map(|entry| DiscoveryMetadataEntry {
            key: entry.key.clone(),
            value: entry.value.clone(),
        })
        .collect();
    let materially_unchanged = existing.is_some_and(|existing| {
        existing.backend_id == device.backend_id
            && existing.device_id == device.device_id
            && existing.display_name == device.display_name
            && existing.availability == availability
            && existing.connection_generation == connection_generation
            && existing.discovery_generation == discovery_generation
            && existing.metadata == metadata
    });
    let device_revision = if materially_unchanged {
        existing.expect("checked above").device_revision
    } else {
        // Filled with the core-owned global revision when the change is
        // committed. The manager task cannot expose this transient value.
        0
    };
    DeviceInfo {
        backend_id: device.backend_id.clone(),
        device_id: device.device_id.clone(),
        display_name: device.display_name.clone(),
        availability,
        connection_generation,
        discovery_generation,
        device_revision,
        metadata,
    }
}

pub(super) fn configured_device_update(
    current: &DeviceInfo,
    event: &InventoryEvent,
) -> Option<DeviceInfo> {
    match event {
        InventoryEvent::DeviceAdded { device, .. }
        | InventoryEvent::DeviceChanged { device, .. }
            if current.backend_id == device.backend_id && current.device_id == device.device_id =>
        {
            Some(device.clone())
        }
        InventoryEvent::DeviceRemoved {
            inventory_revision,
            backend_id,
            device_id,
        } if current.backend_id == *backend_id && current.device_id == *device_id => {
            let mut unavailable = current.clone();
            unavailable.availability = DeviceAvailability::Unavailable;
            unavailable.device_revision = *inventory_revision;
            Some(unavailable)
        }
        _ => None,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum AggregateError {
    #[error("device inventory generation/revision must be nonzero")]
    ZeroGeneration,
    #[error("invalid device: {0}")]
    InvalidDevice(String),
    #[error("device backend ID {actual:?} does not match source {expected:?}")]
    WrongBackendId { expected: String, actual: String },
    #[error("duplicate device {backend_id:?}/{device_id:?}")]
    DuplicateDevice {
        backend_id: String,
        device_id: String,
    },
    #[error("aggregate contains {0} devices; limit is {MAX_PUBLIC_DEVICES}")]
    TooManyDevices(usize),
    #[error("public inventory revision is exhausted")]
    RevisionExhausted,
    #[error(
        "backend {backend_id:?} event has connection generation {actual}; expected {expected}"
    )]
    UnexpectedConnectionGeneration {
        backend_id: String,
        expected: u64,
        actual: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pronk_backend_protocol::{
        BackendInfo, DeviceAvailability as BackendAvailability, DeviceInfo as BackendDevice,
    };

    fn backend_device(backend_id: &str, device_id: &str, name: &str) -> BackendDevice {
        BackendDevice {
            backend_id: backend_id.into(),
            device_id: device_id.into(),
            display_name: name.into(),
            availability: BackendAvailability::Available,
            metadata: Vec::new(),
        }
    }

    fn inventory(
        discovery_generation: u64,
        revision: u64,
        devices: Vec<BackendDevice>,
    ) -> DeviceInventorySnapshot {
        DeviceInventorySnapshot {
            discovery_generation,
            revision,
            devices,
        }
    }

    fn connected(
        connection_generation: u64,
        inventory: DeviceInventorySnapshot,
    ) -> BackendSupervisorEvent {
        BackendSupervisorEvent::Connected {
            connection_generation,
            negotiated_minor: 0,
            info: BackendInfo::new("mock", "Mock", "test", "mock", "development"),
            inventory,
        }
    }

    fn changed(
        connection_generation: u64,
        inventory: DeviceInventorySnapshot,
    ) -> BackendSupervisorEvent {
        BackendSupervisorEvent::InventoryChanged {
            connection_generation,
            inventory,
        }
    }

    #[test]
    fn aggregates_backends_with_one_ordered_public_revision() {
        let mut aggregate = AggregateInventory::default();
        let first = aggregate
            .apply_supervisor_event(
                "alpha",
                &connected(
                    1,
                    inventory(10, 2, vec![backend_device("alpha", "one", "One")]),
                ),
            )
            .unwrap();
        assert!(matches!(
            first,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceAdded { inventory_revision: 1, .. }])
        ));
        aggregate
            .apply_supervisor_event(
                "beta",
                &connected(
                    1,
                    inventory(20, 4, vec![backend_device("beta", "two", "Two")]),
                ),
            )
            .unwrap();

        let snapshot = aggregate.snapshot();
        assert_eq!(snapshot.inventory_revision, 2);
        assert_eq!(snapshot.devices.len(), 2);
        assert_eq!(snapshot.devices[0].backend_id, "alpha");
        assert_eq!(snapshot.devices[1].backend_id, "beta");
        snapshot.validate().unwrap();
    }

    #[test]
    fn disconnect_and_reconnect_preserve_identity_with_new_generations() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    7,
                    inventory(
                        3,
                        2,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        let mut unavailable = inventory(
            3,
            2,
            vec![backend_device("mock", "living-room", "Living Room")],
        );
        unavailable.devices[0].availability = BackendAvailability::Unavailable;
        let disconnect = BackendSupervisorEvent::Disconnected {
            connection_generation: 7,
            reason: pronk_backend_host::BackendDisconnectReason::ConnectionClosed,
            unavailable_inventory: unavailable,
        };
        assert!(matches!(
            aggregate
                .apply_supervisor_event("mock", &disconnect)
                .unwrap(),
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceChanged { inventory_revision: 2, device }]
                    if device.device_revision == 2
                        && device.availability == DeviceAvailability::Unavailable)
        ));

        aggregate
            .apply_supervisor_event(
                "mock",
                &BackendSupervisorEvent::Connecting {
                    connection_generation: 8,
                },
            )
            .unwrap();
        assert_eq!(
            aggregate
                .apply_supervisor_event("mock", &changed(7, inventory(3, 3, Vec::new())))
                .unwrap(),
            ApplySupervisorOutcome::IgnoredStale
        );
        let reconnect = aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    8,
                    inventory(
                        4,
                        2,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        assert!(matches!(
            reconnect,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceChanged { inventory_revision: 3, device }]
                    if device.connection_generation == 8
                        && device.discovery_generation == 4
                        && device.device_revision == 3
                        && device.availability == DeviceAvailability::Available)
        ));
    }

    #[test]
    fn unchanged_devices_keep_their_exact_device_revision() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    1,
                    inventory(
                        1,
                        2,
                        vec![
                            backend_device("mock", "one", "One"),
                            backend_device("mock", "two", "Two"),
                        ],
                    ),
                ),
            )
            .unwrap();
        let outcome = aggregate
            .apply_supervisor_event(
                "mock",
                &changed(
                    1,
                    inventory(
                        1,
                        3,
                        vec![
                            backend_device("mock", "one", "One renamed"),
                            backend_device("mock", "two", "Two"),
                        ],
                    ),
                ),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceChanged { device, .. }]
                    if device.device_id == "one" && device.device_revision == 3)
        ));
        let snapshot = aggregate.snapshot();
        assert_eq!(snapshot.devices[0].device_revision, 3);
        assert_eq!(snapshot.devices[1].device_revision, 2);
    }

    #[test]
    fn readded_identity_gets_a_fresh_device_revision() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    1,
                    inventory(
                        1,
                        1,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        assert_eq!(aggregate.snapshot().devices[0].device_revision, 1);

        aggregate
            .apply_supervisor_event("mock", &changed(1, inventory(1, 2, Vec::new())))
            .unwrap();
        let readded = aggregate
            .apply_supervisor_event(
                "mock",
                &changed(
                    1,
                    inventory(
                        1,
                        3,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        assert!(matches!(
            readded,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceAdded { inventory_revision: 3, device }]
                    if device.device_revision == 3)
        ));
    }

    #[test]
    fn configured_device_state_survives_removal_and_tracks_readdition() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    1,
                    inventory(
                        1,
                        1,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        let current = aggregate.snapshot().devices.remove(0);
        let removal = aggregate
            .apply_supervisor_event("mock", &changed(1, inventory(1, 2, Vec::new())))
            .unwrap();
        let ApplySupervisorOutcome::Changed(removal_events) = removal else {
            panic!("Device removal was ignored");
        };
        let unavailable = configured_device_update(&current, &removal_events[0]).unwrap();
        assert_eq!(unavailable.display_name, "Living Room");
        assert_eq!(unavailable.availability, DeviceAvailability::Unavailable);
        assert_eq!(unavailable.device_revision, 2);
        assert_eq!(aggregate.configured_device(&current), unavailable);

        let readded = aggregate
            .apply_supervisor_event(
                "mock",
                &changed(
                    1,
                    inventory(2, 3, vec![backend_device("mock", "living-room", "Den TV")]),
                ),
            )
            .unwrap();
        let ApplySupervisorOutcome::Changed(readded_events) = readded else {
            panic!("Device readdition was ignored");
        };
        let available = configured_device_update(&unavailable, &readded_events[0]).unwrap();
        assert_eq!(available.display_name, "Den TV");
        assert_eq!(available.availability, DeviceAvailability::Available);
        assert_eq!(available.discovery_generation, 2);
        assert_eq!(available.device_revision, 3);

        let unrelated = InventoryEvent::DeviceRemoved {
            inventory_revision: 4,
            backend_id: "mock".into(),
            device_id: "bedroom".into(),
        };
        assert!(configured_device_update(&available, &unrelated).is_none());
    }

    #[test]
    fn aggregate_bound_rejects_a_whole_snapshot_atomically() {
        let mut aggregate = AggregateInventory::default();
        let full: Vec<_> = (0..MAX_PUBLIC_DEVICES)
            .map(|index| backend_device("alpha", &format!("device-{index}"), "Device"))
            .collect();
        aggregate
            .apply_supervisor_event("alpha", &connected(1, inventory(1, 1, full)))
            .unwrap();
        let before = aggregate.snapshot();
        assert_eq!(
            aggregate
                .apply_supervisor_event(
                    "beta",
                    &connected(
                        1,
                        inventory(1, 1, vec![backend_device("beta", "extra", "Extra")]),
                    ),
                )
                .unwrap_err(),
            AggregateError::TooManyDevices(MAX_PUBLIC_DEVICES + 1)
        );
        assert_eq!(aggregate.snapshot(), before);
    }

    #[test]
    fn resolves_only_the_exact_available_device_revision() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    7,
                    inventory(
                        11,
                        1,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        let device = aggregate.snapshot().devices.remove(0);
        let selection = DeviceSelection::from_device(&device);
        assert_eq!(aggregate.resolve_device(&selection).unwrap(), device);

        for stale in [
            DeviceSelection {
                connection_generation: 8,
                ..selection.clone()
            },
            DeviceSelection {
                discovery_generation: 12,
                ..selection.clone()
            },
            DeviceSelection {
                device_revision: 2,
                ..selection.clone()
            },
        ] {
            assert!(matches!(
                aggregate.resolve_device(&stale),
                Err(ResolveDeviceError::StaleSelection { .. })
            ));
        }

        let mut unavailable = inventory(
            11,
            2,
            vec![backend_device("mock", "living-room", "Living Room")],
        );
        unavailable.devices[0].availability = BackendAvailability::Busy;
        aggregate
            .apply_supervisor_event("mock", &changed(7, unavailable))
            .unwrap();
        let current = DeviceSelection::from_device(&aggregate.snapshot().devices[0]);
        assert!(matches!(
            aggregate.resolve_device(&current),
            Err(ResolveDeviceError::Unavailable {
                availability: DeviceAvailability::Busy,
                ..
            })
        ));
    }
}
