//! Manager-owned reservation state for explicitly added cast displays.

use std::collections::{BTreeMap, HashSet};

use pronk_core::output::{CastKmsOutput, CastKmsOutputId};
use pronk_dbus::DeviceInfo;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DeviceKey {
    backend_id: String,
    device_id: String,
}

impl DeviceKey {
    pub(crate) fn from_device(device: &DeviceInfo) -> Self {
        Self {
            backend_id: device.backend_id.clone(),
            device_id: device.device_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputReservation {
    token: u64,
    device: DeviceKey,
    output: CastKmsOutput,
}

impl OutputReservation {
    pub(crate) fn output(&self) -> &CastKmsOutput {
        &self.output
    }

    pub(crate) fn release(&self) -> OutputReservationRelease {
        OutputReservationRelease {
            token: self.token,
            device: self.device.clone(),
            output_id: self.output.id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputReservationRelease {
    token: u64,
    device: DeviceKey,
    output_id: CastKmsOutputId,
}

#[derive(Debug, Default)]
pub(crate) struct OutputSlotPool {
    next_token: u64,
    claims: BTreeMap<CastKmsOutputId, OutputClaim>,
    devices: BTreeMap<DeviceKey, CastKmsOutputId>,
}

#[derive(Debug)]
struct OutputClaim {
    token: u64,
    device: DeviceKey,
}

impl OutputSlotPool {
    pub(crate) fn reserve(
        &mut self,
        device: &DeviceInfo,
        outputs: &[CastKmsOutput],
        preferred: Option<&CastKmsOutputId>,
    ) -> Result<OutputReservation, OutputReservationError> {
        let device_key = DeviceKey::from_device(device);
        if self.devices.contains_key(&device_key) {
            return Err(OutputReservationError::DeviceAlreadyClaimed {
                backend_id: device.backend_id.clone(),
                device_id: device.device_id.clone(),
            });
        }
        validate_inventory(outputs)?;

        let available = |output: &&CastKmsOutput| {
            output.is_available() && !self.claims.contains_key(&output.id)
        };
        let selected = preferred
            .and_then(|preferred| {
                outputs
                    .iter()
                    .find(|output| &output.id == preferred && available(output))
            })
            .or_else(|| {
                outputs
                    .iter()
                    .filter(available)
                    .min_by(|left, right| left.id.cmp(&right.id))
            })
            .ok_or(OutputReservationError::CapacityExhausted)?;

        let token = self
            .next_token
            .checked_add(1)
            .ok_or(OutputReservationError::TokenExhausted)?;
        self.next_token = token;
        let previous = self.claims.insert(
            selected.id.clone(),
            OutputClaim {
                token,
                device: device_key.clone(),
            },
        );
        debug_assert!(previous.is_none());
        let previous = self.devices.insert(device_key.clone(), selected.id.clone());
        debug_assert!(previous.is_none());
        Ok(OutputReservation {
            token,
            device: device_key,
            output: selected.clone(),
        })
    }

    /// Release only the exact live reservation named by this cleanup message.
    ///
    /// A delayed cleanup from an older operation must never release a slot
    /// that has since been reserved again, hence the token and device checks.
    pub(crate) fn release(&mut self, release: &OutputReservationRelease) -> bool {
        let matches = self
            .claims
            .get(&release.output_id)
            .is_some_and(|claim| claim.token == release.token && claim.device == release.device);
        if !matches {
            return false;
        }
        self.claims.remove(&release.output_id);
        self.devices.remove(&release.device);
        true
    }

    #[cfg(test)]
    pub(crate) fn claim_count(&self) -> usize {
        self.claims.len()
    }
}

fn validate_inventory(outputs: &[CastKmsOutput]) -> Result<(), OutputReservationError> {
    let mut identities = HashSet::with_capacity(outputs.len());
    let mut connectors = HashSet::with_capacity(outputs.len());
    for output in outputs {
        if !output.node_path.is_absolute()
            || !output.id.device_path.is_absolute()
            || output.device_major == 0
            || output.connector_id == 0
            || output.connector_name.is_empty()
        {
            return Err(OutputReservationError::InvalidInventory);
        }
        if !identities.insert(output.id.clone()) {
            return Err(OutputReservationError::InvalidInventory);
        }
        if !connectors.insert((output.id.device_path.clone(), output.connector_id)) {
            return Err(OutputReservationError::InvalidInventory);
        }
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OutputReservationError {
    #[error("CastKMS output inventory is invalid")]
    InvalidInventory,
    #[error("device {backend_id:?}/{device_id:?} already owns or is reserving a display slot")]
    DeviceAlreadyClaimed {
        backend_id: String,
        device_id: String,
    },
    #[error("no disconnected CastKMS output is available")]
    CapacityExhausted,
    #[error("display-slot reservation generation is exhausted")]
    TokenExhausted,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pronk_core::output::OutputConnection;
    use pronk_dbus::DeviceAvailability;

    use super::*;

    fn device(id: &str) -> DeviceInfo {
        DeviceInfo {
            backend_id: "mock".into(),
            device_id: id.into(),
            display_name: format!("Device {id}"),
            availability: DeviceAvailability::Available,
            connection_generation: 1,
            discovery_generation: 2,
            device_revision: 3,
            metadata: Vec::new(),
        }
    }

    fn output(index: u32, connection: OutputConnection) -> CastKmsOutput {
        CastKmsOutput {
            id: CastKmsOutputId {
                device_path: PathBuf::from("/sys/devices/virtual/castkms"),
                output_index: index,
            },
            node_path: PathBuf::from("/dev/dri/card9"),
            device_major: 226,
            device_minor: 9,
            connector_id: index + 40,
            connector_name: format!("Virtual-{}", index + 1),
            connection,
        }
    }

    #[test]
    fn reserves_only_disconnected_slots_in_stable_order() {
        let outputs = vec![
            output(2, OutputConnection::Disconnected),
            output(0, OutputConnection::Connected),
            output(1, OutputConnection::Disconnected),
        ];
        let mut pool = OutputSlotPool::default();
        let first = pool.reserve(&device("one"), &outputs, None).unwrap();
        let second = pool.reserve(&device("two"), &outputs, None).unwrap();
        assert_eq!(first.output().id.output_index, 1);
        assert_eq!(second.output().id.output_index, 2);
        assert_eq!(pool.claim_count(), 2);
        assert_eq!(
            pool.reserve(&device("three"), &outputs, None),
            Err(OutputReservationError::CapacityExhausted)
        );
    }

    #[test]
    fn honors_a_free_affinity_but_never_an_unavailable_one() {
        let outputs = vec![
            output(0, OutputConnection::Disconnected),
            output(1, OutputConnection::Connected),
            output(2, OutputConnection::Disconnected),
        ];
        let mut pool = OutputSlotPool::default();
        let preferred = outputs[2].id.clone();
        let reservation = pool
            .reserve(&device("one"), &outputs, Some(&preferred))
            .unwrap();
        assert_eq!(reservation.output().id, preferred);

        let unavailable = outputs[1].id.clone();
        let fallback = pool
            .reserve(&device("two"), &outputs, Some(&unavailable))
            .unwrap();
        assert_eq!(fallback.output().id.output_index, 0);
    }

    #[test]
    fn exact_cleanup_is_stale_safe_and_frees_device_capacity() {
        let outputs = vec![output(0, OutputConnection::Disconnected)];
        let mut pool = OutputSlotPool::default();
        let first = pool.reserve(&device("one"), &outputs, None).unwrap();
        let mut stale = first.release();
        stale.token += 1;
        assert!(!pool.release(&stale));
        assert_eq!(pool.claim_count(), 1);
        assert!(pool.release(&first.release()));
        assert_eq!(pool.claim_count(), 0);

        let second = pool.reserve(&device("one"), &outputs, None).unwrap();
        assert_ne!(first.token, second.token);
    }

    #[test]
    fn rejects_duplicate_devices_and_malformed_inventories() {
        let outputs = vec![output(0, OutputConnection::Disconnected)];
        let mut pool = OutputSlotPool::default();
        let reservation = pool.reserve(&device("one"), &outputs, None).unwrap();
        assert!(matches!(
            pool.reserve(&device("one"), &outputs, None),
            Err(OutputReservationError::DeviceAlreadyClaimed { .. })
        ));
        assert!(pool.release(&reservation.release()));

        let duplicate = vec![outputs[0].clone(), outputs[0].clone()];
        assert_eq!(
            pool.reserve(&device("two"), &duplicate, None),
            Err(OutputReservationError::InvalidInventory)
        );
    }
}
