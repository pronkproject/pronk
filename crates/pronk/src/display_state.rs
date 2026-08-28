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
pub struct DisplayTopology {
    pub attachment: AttachmentState,
    pub route: Option<ActiveRoute>,
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
pub struct DisplayRuntimeState {
    pub revision: u64,
    pub route_generation: u64,
    pub attachment: AttachmentState,
    pub route: RouteState,
    pub media_generation: u64,
    pub media: MediaState,
    pub last_error: Option<String>,
}

impl DisplayRuntimeState {
    pub fn attached(initial_revision: u64) -> Self {
        Self {
            revision: initial_revision.max(1),
            route_generation: 0,
            attachment: AttachmentState::Attached,
            route: RouteState::Disabled,
            media_generation: 0,
            media: MediaState::Idle,
            last_error: None,
        }
    }

    /// Apply one authoritative kernel topology observation.
    ///
    /// A non-attached connector can never retain an active route. Media is a
    /// separate child-actor projection and is not guessed from topology.
    pub fn observe_topology(&mut self, topology: DisplayTopology) -> bool {
        let attachment = topology.attachment;
        let route = if attachment == AttachmentState::Attached {
            topology
                .route
                .map_or(RouteState::Disabled, RouteState::Active)
        } else {
            RouteState::Disabled
        };
        if self.attachment == attachment && self.route == route {
            return false;
        }
        self.attachment = attachment;
        if self.route != route {
            self.route_generation = self.route_generation.saturating_add(1);
        }
        self.route = route;
        self.advance();
        true
    }

    pub fn observe_media(
        &mut self,
        generation: u64,
        state: MediaState,
        last_error: Option<String>,
    ) -> bool {
        if self.media_generation == generation
            && self.media == state
            && self.last_error == last_error
        {
            return false;
        }
        self.media_generation = generation;
        self.media = state;
        self.last_error = last_error;
        self.advance();
        true
    }

    pub fn advance_to_at_least(&mut self, revision: u64) {
        if revision > self.revision {
            self.revision = revision;
        }
    }

    fn advance(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_topology(width: u32) -> DisplayTopology {
        DisplayTopology {
            attachment: AttachmentState::Attached,
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
            state.route,
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
        moved.route.as_mut().unwrap().target = RouteTarget::new(NonZeroU32::new(8).unwrap());

        assert!(state.observe_topology(moved));
        assert_eq!(state.route_generation, 2);
    }

    #[test]
    fn detachment_clears_an_active_route_but_not_child_media_by_fiat() {
        let mut state = DisplayRuntimeState::attached(1);
        state.observe_topology(active_topology(1920));
        state.observe_media(1, MediaState::Running, None);
        let revision = state.revision;

        assert!(state.observe_topology(DisplayTopology {
            attachment: AttachmentState::Detached,
            route: Some(active_topology(1920).route.unwrap()),
        }));
        assert_eq!(state.attachment, AttachmentState::Detached);
        assert_eq!(state.route, RouteState::Disabled);
        assert_eq!(state.media_generation, 1);
        assert_eq!(state.media, MediaState::Running);
        assert_eq!(state.revision, revision + 1);
        assert_eq!(state.route_generation, 2);
    }

    #[test]
    fn media_errors_are_cleared_by_a_successful_transition() {
        let mut state = DisplayRuntimeState::attached(1);
        assert!(state.observe_media(1, MediaState::Failed, Some("network lost".into())));
        assert!(state.observe_media(2, MediaState::StartingCapture, None));
        assert_eq!(state.media_generation, 2);
        assert_eq!(state.last_error, None);
        assert!(!state.observe_media(2, MediaState::StartingCapture, None));
    }
}
