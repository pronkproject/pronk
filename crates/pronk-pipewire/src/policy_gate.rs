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
pub(crate) enum PolicyGate {
    #[default]
    Ambient,
    WaitingForMarker,
    Marked(u32),
}

impl PolicyGate {
    pub(crate) fn new(required: bool) -> Self {
        if required {
            Self::WaitingForMarker
        } else {
            Self::Ambient
        }
    }

    pub(crate) fn observe_metadata(&mut self, object_id: u32, name: Option<&str>) {
        if matches!(self, Self::WaitingForMarker) && name == Some(POLICY_METADATA_NAME) {
            *self = Self::Marked(object_id);
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self, Self::Ambient | Self::Marked(_))
    }

    pub(crate) fn remove_object(&mut self, object_id: u32) -> PolicyMarkerChange {
        match self {
            Self::Marked(marker_id) if *marker_id == object_id => {
                *self = Self::WaitingForMarker;
                PolicyMarkerChange::Lost
            }
            Self::Ambient | Self::WaitingForMarker | Self::Marked(_) => {
                PolicyMarkerChange::Unchanged
            }
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
