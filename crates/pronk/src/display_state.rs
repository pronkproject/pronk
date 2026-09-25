//! Pure cast-display state and transition rules.
//!
//! This module deliberately knows nothing about D-Bus, Tokio, CastKMS, or
//! backend transports. Actors feed it validated observations and adapters
//! project its snapshots outward.

use std::num::NonZeroU32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentState {
    Attached,
    Detached,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutedMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteTarget(NonZeroU32);

impl RouteTarget {
    pub fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    pub fn as_nonzero(self) -> NonZeroU32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveRoute {
    pub target: RouteTarget,
    pub mode: RoutedMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayTopology {
    Attached { route: Option<ActiveRoute> },
    Detached,
    Unknown,
}

impl DisplayTopology {
    fn route(self) -> RouteState {
        match self {
            Self::Attached { route: Some(route) } => RouteState::Active(route),
            Self::Attached { route: None } | Self::Detached | Self::Unknown => RouteState::Disabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteState {
    Disabled,
    Active(ActiveRoute),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayGrantState {
    Pending,
    Active,
    SuspendedNoMaster,
    SuspendedOtherMaster,
    SuspendedForeignContent,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    Idle,
    StartingCapture,
    StartingMedia,
    Running,
    Suspended,
    Reconfiguring,
    Reconnecting,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaStatus {
    Idle,
    StartingCapture,
    StartingMedia,
    Running,
    Suspended,
    Reconfiguring,
    Reconnecting,
    Stopping,
    Failed(String),
}

impl MediaStatus {
    pub fn state(&self) -> MediaState {
        match self {
            Self::Idle => MediaState::Idle,
            Self::StartingCapture => MediaState::StartingCapture,
            Self::StartingMedia => MediaState::StartingMedia,
            Self::Running => MediaState::Running,
            Self::Suspended => MediaState::Suspended,
            Self::Reconfiguring => MediaState::Reconfiguring,
            Self::Reconnecting => MediaState::Reconnecting,
            Self::Stopping => MediaState::Stopping,
            Self::Failed(_) => MediaState::Failed,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRuntimeState {
    revision: u64,
    route_generation: u64,
    topology: DisplayTopology,
    media_generation: u64,
    media: MediaStatus,
}

impl DisplayRuntimeState {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn route_generation(&self) -> u64 {
        self.route_generation
    }
    pub fn attachment(&self) -> AttachmentState {
        match self.topology {
            DisplayTopology::Attached { .. } => AttachmentState::Attached,
            DisplayTopology::Detached => AttachmentState::Detached,
            DisplayTopology::Unknown => AttachmentState::Unknown,
        }
    }
    pub fn route(&self) -> RouteState {
        self.topology.route()
    }
    pub fn media_generation(&self) -> u64 {
        self.media_generation
    }
    pub fn media(&self) -> MediaState {
        self.media.state()
    }
    pub fn last_error(&self) -> Option<&str> {
        self.media.error()
    }

    pub fn attached(initial_revision: u64) -> Self {
        Self {
            revision: initial_revision.max(1),
            route_generation: 0,
            topology: DisplayTopology::Attached { route: None },
            media_generation: 0,
            media: MediaStatus::Idle,
        }
    }

    /// Apply one authoritative kernel topology observation.
    ///
    /// A non-attached connector can never retain an active route. Media is a
    /// separate child-actor projection and is not guessed from topology.
    pub fn observe_topology(&mut self, topology: DisplayTopology) -> bool {
        if self.topology == topology {
            return false;
        }
        if self.route() != topology.route() {
            self.route_generation = self.route_generation.saturating_add(1);
        }
        self.topology = topology;
        self.advance();
        true
    }

    pub fn observe_media(&mut self, generation: u64, status: MediaStatus) -> bool {
        if generation < self.media_generation {
            return false;
        }
        if self.media_generation == generation && self.media == status {
            return false;
        }
        self.media_generation = generation;
        self.media = status;
        self.advance();
        true
    }

    pub fn advance_to_at_least(&mut self, revision: u64) {
        if revision > self.revision {
            self.revision = revision;
        }
    }

    pub fn advance_for_external_change(&mut self, minimum_revision: u64) {
        self.revision = self.revision.saturating_add(1).max(minimum_revision);
    }

    fn advance(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_topology(width: u32) -> DisplayTopology {
        DisplayTopology::Attached {
            route: Some(ActiveRoute {
                target: RouteTarget::new(NonZeroU32::new(7).unwrap()),
                mode: RoutedMode {
                    width,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            }),
        }
    }

    #[test]
    fn topology_is_revisioned_only_on_material_change() {
        let mut state = DisplayRuntimeState::attached(10);
        assert!(state.observe_topology(active_topology(1920)));
        assert_eq!(state.revision, 11);
        assert_eq!(
            state.route(),
            RouteState::Active(ActiveRoute {
                target: RouteTarget::new(NonZeroU32::new(7).unwrap()),
                mode: RoutedMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            })
        );
        assert_eq!(state.route_generation, 1);
        assert!(!state.observe_topology(active_topology(1920)));
        assert_eq!(state.revision, 11);
        assert_eq!(state.route_generation, 1);

        assert!(state.observe_topology(active_topology(1280)));
        assert_eq!(state.revision, 12);
        assert_eq!(state.route_generation, 2);
    }

    #[test]
    fn a_same_mode_route_target_change_is_material() {
        let mut state = DisplayRuntimeState::attached(1);
        state.observe_topology(active_topology(1920));
        let mut moved = active_topology(1920);
        let DisplayTopology::Attached { route: Some(route) } = &mut moved else {
            unreachable!("test topology is active");
        };
        route.target = RouteTarget::new(NonZeroU32::new(8).unwrap());

        assert!(state.observe_topology(moved));
        assert_eq!(state.route_generation, 2);
    }

    #[test]
    fn detachment_clears_an_active_route_but_not_child_media_by_fiat() {
        let mut state = DisplayRuntimeState::attached(1);
        state.observe_topology(active_topology(1920));
        state.observe_media(1, MediaStatus::Running);
        let revision = state.revision;

        assert!(state.observe_topology(DisplayTopology::Detached));
        assert_eq!(state.attachment(), AttachmentState::Detached);
        assert_eq!(state.route(), RouteState::Disabled);
        assert_eq!(state.media_generation, 1);
        assert_eq!(state.media(), MediaState::Running);
        assert_eq!(state.revision, revision + 1);
        assert_eq!(state.route_generation, 2);
    }

    #[test]
    fn attachment_changes_without_a_route_do_not_advance_route_generation() {
        let mut state = DisplayRuntimeState::attached(1);
        for topology in [
            DisplayTopology::Detached,
            DisplayTopology::Unknown,
            DisplayTopology::Attached { route: None },
        ] {
            assert!(state.observe_topology(topology));
            assert_eq!(state.route(), RouteState::Disabled);
            assert_eq!(state.route_generation(), 0);
        }
        assert_eq!(state.attachment(), AttachmentState::Attached);
    }

    #[test]
    fn media_errors_are_cleared_by_a_successful_transition() {
        let mut state = DisplayRuntimeState::attached(1);
        assert!(state.observe_media(1, MediaStatus::Failed("network lost".into())));
        assert!(state.observe_media(2, MediaStatus::StartingCapture));
        assert_eq!(state.media_generation, 2);
        assert_eq!(state.last_error(), None);
        assert!(!state.observe_media(2, MediaStatus::StartingCapture));
    }

    #[test]
    fn older_media_generations_cannot_replace_a_newer_projection() {
        let mut state = DisplayRuntimeState::attached(1);
        state.observe_media(2, MediaStatus::Running);
        let revision = state.revision();

        assert!(!state.observe_media(1, MediaStatus::Failed("stale".into())));
        assert_eq!(state.media_generation(), 2);
        assert_eq!(state.media(), MediaState::Running);
        assert_eq!(state.revision(), revision);
    }
}
