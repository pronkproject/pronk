use std::collections::BTreeMap;

use pronk_backend_protocol::{DeviceIdentity, DeviceInfo, DeviceSnapshot, Validate};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Added {
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    },
    Changed {
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    },
    Removed {
        discovery_generation: u64,
        revision: u64,
        device: DeviceIdentity,
    },
}

impl DeviceEvent {
    pub fn discovery_generation(&self) -> u64 {
        match self {
            Self::Added {
                discovery_generation,
                ..
            }
            | Self::Changed {
                discovery_generation,
                ..
            }
            | Self::Removed {
                discovery_generation,
                ..
            } => *discovery_generation,
        }
    }

    pub fn revision(&self) -> u64 {
        match self {
            Self::Added { revision, .. }
            | Self::Changed { revision, .. }
            | Self::Removed { revision, .. } => *revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInventorySnapshot {
    pub discovery_generation: u64,
    pub revision: u64,
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug)]
pub struct DeviceInventory {
    expected_backend_id: String,
    discovery_generation: u64,
    baseline_revision: u64,
    revision: u64,
    devices: BTreeMap<(String, String), DeviceInfo>,
    last_applied: Option<DeviceEvent>,
}

impl DeviceInventory {
    pub fn from_snapshot(
        expected_backend_id: impl Into<String>,
        snapshot: DeviceSnapshot,
    ) -> Result<Self, InventoryError> {
        let expected_backend_id = expected_backend_id.into();
        snapshot
            .validate()
            .map_err(|error| InventoryError::InvalidRecord(error.to_string()))?;
        let mut devices = BTreeMap::new();
        for device in snapshot.devices {
            require_backend(&expected_backend_id, &device.backend_id)?;
            devices.insert(device_key(&device), device);
        }
        Ok(Self {
            expected_backend_id,
            discovery_generation: snapshot.discovery_generation,
            baseline_revision: snapshot.revision,
            revision: snapshot.revision,
            devices,
            last_applied: None,
        })
    }

    pub fn snapshot(&self) -> DeviceInventorySnapshot {
        DeviceInventorySnapshot {
            discovery_generation: self.discovery_generation,
            revision: self.revision,
            devices: self.devices.values().cloned().collect(),
        }
    }

    pub fn apply(&mut self, event: DeviceEvent) -> Result<ApplyOutcome, InventoryError> {
        validate_event(&self.expected_backend_id, &event)?;
        if event.discovery_generation() != self.discovery_generation {
            return Err(InventoryError::GenerationMismatch {
                expected: self.discovery_generation,
                actual: event.discovery_generation(),
            });
        }

        let event_revision = event.revision();
        if event_revision <= self.baseline_revision || event_revision < self.revision {
            return Ok(ApplyOutcome::IgnoredCoveredBySnapshot);
        }
        if event_revision == self.revision {
            return if self.last_applied.as_ref() == Some(&event) {
                Ok(ApplyOutcome::IgnoredDuplicate)
            } else {
                Err(InventoryError::ConflictingDuplicate(event_revision))
            };
        }
        let expected_revision = self
            .revision
            .checked_add(1)
            .ok_or(InventoryError::RevisionExhausted)?;
        if event_revision != expected_revision {
            return Err(InventoryError::RevisionGap {
                expected: expected_revision,
                actual: event_revision,
            });
        }

        match &event {
            DeviceEvent::Added { device, .. } => {
                let key = device_key(device);
                if self.devices.contains_key(&key) {
                    return Err(InventoryError::AlreadyPresent {
                        backend_id: key.0,
                        device_id: key.1,
                    });
                }
                self.devices.insert(key, device.clone());
            }
            DeviceEvent::Changed { device, .. } => {
                let key = device_key(device);
                if !self.devices.contains_key(&key) {
                    return Err(InventoryError::NotPresent {
                        backend_id: key.0,
                        device_id: key.1,
                    });
                }
                self.devices.insert(key, device.clone());
            }
            DeviceEvent::Removed { device, .. } => {
                let key = (device.backend_id.clone(), device.device_id.clone());
                if self.devices.remove(&key).is_none() {
                    return Err(InventoryError::NotPresent {
                        backend_id: key.0,
                        device_id: key.1,
                    });
                }
            }
        }
        self.revision = event_revision;
        self.last_applied = Some(event);
        Ok(ApplyOutcome::Changed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Changed,
    IgnoredCoveredBySnapshot,
    IgnoredDuplicate,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InventoryError {
    #[error("invalid device record: {0}")]
    InvalidRecord(String),
    #[error("device backend ID {actual:?} does not match endpoint {expected:?}")]
    WrongBackendId { expected: String, actual: String },
    #[error("discovery generation {actual} does not match active generation {expected}")]
    GenerationMismatch { expected: u64, actual: u64 },
    #[error("device revision gap: expected {expected}, received {actual}")]
    RevisionGap { expected: u64, actual: u64 },
    #[error("device revision counter is exhausted")]
    RevisionExhausted,
    #[error("revision {0} was repeated with different contents")]
    ConflictingDuplicate(u64),
    #[error("device {backend_id:?}/{device_id:?} was added twice")]
    AlreadyPresent {
        backend_id: String,
        device_id: String,
    },
    #[error("device {backend_id:?}/{device_id:?} is not in the inventory")]
    NotPresent {
        backend_id: String,
        device_id: String,
    },
}

fn validate_event(expected_backend_id: &str, event: &DeviceEvent) -> Result<(), InventoryError> {
    if event.revision() == 0 {
        return Err(InventoryError::InvalidRecord(
            "event revision must be nonzero".into(),
        ));
    }
    match event {
        DeviceEvent::Added { device, .. } | DeviceEvent::Changed { device, .. } => {
            device
                .validate()
                .map_err(|error| InventoryError::InvalidRecord(error.to_string()))?;
            require_backend(expected_backend_id, &device.backend_id)
        }
        DeviceEvent::Removed { device, .. } => {
            device
                .validate()
                .map_err(|error| InventoryError::InvalidRecord(error.to_string()))?;
            require_backend(expected_backend_id, &device.backend_id)
        }
    }
}

fn require_backend(expected: &str, actual: &str) -> Result<(), InventoryError> {
    if expected != actual {
        return Err(InventoryError::WrongBackendId {
            expected: expected.into(),
            actual: actual.into(),
        });
    }
    Ok(())
}

fn device_key(device: &DeviceInfo) -> (String, String) {
    (device.backend_id.clone(), device.device_id.clone())
}

#[cfg(test)]
mod tests {
    use pronk_backend_protocol::{DeviceAvailability, DeviceSnapshot};

    use super::*;

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            backend_id: "mock".into(),
            device_id: id.into(),
            display_name: name.into(),
            availability: DeviceAvailability::Available,
            metadata: Vec::new(),
        }
    }

    fn inventory() -> DeviceInventory {
        DeviceInventory::from_snapshot(
            "mock",
            DeviceSnapshot {
                discovery_generation: 3,
                revision: 2,
                devices: vec![device("one", "One"), device("two", "Two")],
            },
        )
        .unwrap()
    }

    #[test]
    fn snapshot_covers_queued_initial_signals() {
        let mut inventory = inventory();
        assert_eq!(
            inventory
                .apply(DeviceEvent::Added {
                    discovery_generation: 3,
                    revision: 1,
                    device: device("one", "One"),
                })
                .unwrap(),
            ApplyOutcome::IgnoredCoveredBySnapshot
        );
        assert_eq!(inventory.snapshot().revision, 2);
    }

    #[test]
    fn applies_contiguous_changes_and_exact_duplicates() {
        let mut inventory = inventory();
        let changed = DeviceEvent::Changed {
            discovery_generation: 3,
            revision: 3,
            device: device("one", "One renamed"),
        };
        assert_eq!(
            inventory.apply(changed.clone()).unwrap(),
            ApplyOutcome::Changed
        );
        assert_eq!(
            inventory.apply(changed).unwrap(),
            ApplyOutcome::IgnoredDuplicate
        );
        assert_eq!(inventory.snapshot().devices[0].display_name, "One renamed");

        assert_eq!(
            inventory
                .apply(DeviceEvent::Removed {
                    discovery_generation: 3,
                    revision: 4,
                    device: DeviceIdentity {
                        backend_id: "mock".into(),
                        device_id: "two".into(),
                    },
                })
                .unwrap(),
            ApplyOutcome::Changed
        );
        assert_eq!(inventory.snapshot().devices.len(), 1);
    }

    #[test]
    fn gaps_generation_changes_and_conflicting_duplicates_require_resnapshot() {
        let mut inventory = inventory();
        assert!(matches!(
            inventory.apply(DeviceEvent::Added {
                discovery_generation: 3,
                revision: 4,
                device: device("three", "Three"),
            }),
            Err(InventoryError::RevisionGap {
                expected: 3,
                actual: 4
            })
        ));
        assert!(matches!(
            inventory.apply(DeviceEvent::Added {
                discovery_generation: 4,
                revision: 3,
                device: device("three", "Three"),
            }),
            Err(InventoryError::GenerationMismatch { .. })
        ));

        let applied = DeviceEvent::Changed {
            discovery_generation: 3,
            revision: 3,
            device: device("one", "One renamed"),
        };
        inventory.apply(applied).unwrap();
        assert!(matches!(
            inventory.apply(DeviceEvent::Changed {
                discovery_generation: 3,
                revision: 3,
                device: device("one", "Different duplicate"),
            }),
            Err(InventoryError::ConflictingDuplicate(3))
        ));
    }

    #[test]
    fn rejects_cross_backend_records() {
        let mut wrong = device("one", "One");
        wrong.backend_id = "other".into();
        assert!(matches!(
            DeviceInventory::from_snapshot(
                "mock",
                DeviceSnapshot {
                    discovery_generation: 1,
                    revision: 1,
                    devices: vec![wrong],
                }
            ),
            Err(InventoryError::WrongBackendId { .. })
        ));
    }
}
