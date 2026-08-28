//! Pure state machine for the WirePlumber policy availability gate.
//!
//! The PipeWire runtime owns transport and callback mechanics; this module
//! owns only the decision about whether publishing private media is safe.

pub(crate) const POLICY_METADATA_NAME: &str = "pronk-policy-v1";
pub(crate) const PRIVATE_NODE_PROPERTY: &str = "api.pronk.private";
pub(crate) const PRIVATE_NODE_POLICY_VERSION: &str = "v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyMarkerChange {
    Unchanged,
    Lost,
}

#[derive(Debug, Default)]
pub(crate) struct PolicyGate {
    required: bool,
    marker_id: Option<u32>,
}

impl PolicyGate {
    pub(crate) fn new(required: bool) -> Self {
        Self {
            required,
            marker_id: None,
        }
    }

    pub(crate) fn observe_metadata(&mut self, object_id: u32, name: Option<&str>) {
        if self.required && self.marker_id.is_none() && name == Some(POLICY_METADATA_NAME) {
            self.marker_id = Some(object_id);
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        !self.required || self.marker_id.is_some()
    }

    pub(crate) fn remove_object(&mut self, object_id: u32) -> PolicyMarkerChange {
        if self.marker_id == Some(object_id) {
            self.marker_id = None;
            PolicyMarkerChange::Lost
        } else {
            PolicyMarkerChange::Unchanged
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classified_remote_requires_the_exact_versioned_marker() {
        let mut gate = PolicyGate::new(true);
        assert!(!gate.is_open());

        gate.observe_metadata(40, Some("default"));
        assert!(!gate.is_open());

        gate.observe_metadata(41, Some(POLICY_METADATA_NAME));
        assert!(gate.is_open());
        assert_eq!(gate.remove_object(40), PolicyMarkerChange::Unchanged);
        assert!(gate.is_open());
        assert_eq!(gate.remove_object(41), PolicyMarkerChange::Lost);
        assert!(!gate.is_open());
    }

    #[test]
    fn ambient_development_does_not_depend_on_system_policy() {
        let mut gate = PolicyGate::new(false);
        assert!(gate.is_open());
        gate.observe_metadata(41, Some(POLICY_METADATA_NAME));
        assert!(gate.is_open());
        assert_eq!(gate.remove_object(41), PolicyMarkerChange::Unchanged);
    }
}
