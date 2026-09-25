#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PolicyRegistrySync {
    #[default]
    Unrequested,
    Awaiting(i32),
    Complete,
}

impl PolicyRegistrySync {
    pub(crate) fn matches_done(self, object_id: u32, sequence: i32) -> bool {
        matches!(self, Self::Awaiting(expected) if object_id == 0 && sequence == expected)
    }

    pub(crate) fn is_complete(self) -> bool {
        self == Self::Complete
    }
}

#[cfg(test)]
mod tests {
    use super::PolicyRegistrySync;

    #[test]
    fn policy_barrier_only_accepts_the_requested_core_sequence() {
        assert!(!PolicyRegistrySync::Unrequested.matches_done(0, 7));
        let sync = PolicyRegistrySync::Awaiting(7);
        assert!(!sync.matches_done(1, 7));
        assert!(!sync.matches_done(0, 8));
        assert!(sync.matches_done(0, 7));
        assert!(PolicyRegistrySync::Complete.is_complete());
        assert!(!PolicyRegistrySync::Complete.matches_done(0, 7));
    }
}
