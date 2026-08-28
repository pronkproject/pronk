//! Infrastructure adapter that owns one grant holder and observes its output.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use pronk_core::castkms::{
    ActiveOutputRoute, AsyncCastKmsClient, CastKmsClient, CastKmsEvent,
    GrantState as CastKmsGrantState, MonitorAttachmentState, OutputTopology,
};
use tokio::time::{interval, MissedTickBehavior};

use crate::display_state::{
    ActiveRoute, AttachmentState, DisplayGrantState, DisplayTopology, RouteTarget, RoutedMode,
};
use crate::kernel_display_port::{
    KernelDisplayError, KernelDisplayEvent, KernelDisplayMetadata, KernelDisplayObservation,
    KernelDisplayPort,
};

pub const DEFAULT_TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct CastKmsDisplayMonitor {
    client: Option<AsyncCastKmsClient>,
    current: KernelDisplayObservation,
    pending: VecDeque<KernelDisplayEvent>,
    poll: tokio::time::Interval,
}

impl CastKmsDisplayMonitor {
    pub fn new(client: CastKmsClient) -> Result<Self, KernelDisplayError> {
        Self::with_poll_interval(client, DEFAULT_TOPOLOGY_POLL_INTERVAL)
    }

    pub fn with_poll_interval(
        client: CastKmsClient,
        poll_interval: Duration,
    ) -> Result<Self, KernelDisplayError> {
        validate_poll_interval(poll_interval)?;
        let current = query_observation(&client)?;
        let client = client.into_async().map_err(|error| {
            KernelDisplayError::new(
                "register grant holder for Tokio readiness",
                error.to_string(),
            )
        })?;
        let mut poll = interval(poll_interval);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The initial state is already captured above; do not immediately
        // duplicate it through Interval's eager first tick.
        poll.reset();
        Ok(Self {
            client: Some(client),
            current,
            pending: VecDeque::new(),
            poll,
        })
    }

    fn client(&self) -> &AsyncCastKmsClient {
        self.client
            .as_ref()
            .expect("live CastKMS monitor owns its client")
    }

    fn update(&mut self, observation: KernelDisplayObservation) {
        if observation != self.current {
            self.current = observation;
            self.pending
                .push_back(KernelDisplayEvent::Changed(observation));
        }
    }
}

fn validate_poll_interval(poll_interval: Duration) -> Result<(), KernelDisplayError> {
    if poll_interval.is_zero() {
        return Err(KernelDisplayError::new(
            "configure CastKMS topology monitor",
            "poll interval must be nonzero",
        ));
    }
    Ok(())
}

#[async_trait]
impl KernelDisplayPort for CastKmsDisplayMonitor {
    fn metadata(&self) -> KernelDisplayMetadata {
        KernelDisplayMetadata {
            grant_id: self.client().client().grant_id(),
        }
    }

    fn initial_observation(&self) -> KernelDisplayObservation {
        self.current
    }

    async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }

            tokio::select! {
                _ = self.poll.tick() => {
                    let observation = query_observation(self.client().client())?;
                    self.update(observation);
                }
                result = self.client.as_mut().expect("live monitor owns its client").read_events() => {
                    let events = result.map_err(|error| {
                        KernelDisplayError::new("read grant-holder event stream", error.to_string())
                    })?;
                    let mut refresh = false;
                    for event in events {
                        match event {
                            CastKmsEvent::GrantRevoked(_) => {
                                self.pending.push_back(KernelDisplayEvent::Revoked);
                            }
                            CastKmsEvent::GrantState(_) => refresh = true,
                            CastKmsEvent::CaptureFrame(_) => {
                                // Capture events are consumed by the future
                                // media adapter. Seeing one without an active
                                // capture stream is rejected in pronk-core.
                                refresh = true;
                            }
                            CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                        }
                    }
                    if refresh {
                        let observation = query_observation(self.client().client())?;
                        self.update(observation);
                    }
                }
            }
        }
    }

    async fn detach(mut self: Box<Self>) -> Result<(), KernelDisplayError> {
        let client = self
            .client
            .take()
            .expect("live CastKMS monitor owns its client")
            .into_client();
        tokio::task::spawn_blocking(move || {
            client.detach_monitor().map_err(|error| {
                KernelDisplayError::new("detach grant-scoped monitor", error.to_string())
            })
        })
        .await
        .map_err(|error| KernelDisplayError::new("join CastKMS teardown task", error.to_string()))?
    }
}

pub(crate) fn query_observation(
    client: &CastKmsClient,
) -> Result<KernelDisplayObservation, KernelDisplayError> {
    let grant = client.query_grant().map_err(|error| {
        KernelDisplayError::new("query grant-scoped display state", error.to_string())
    })?;
    let topology = client.query_output_topology().map_err(|error| {
        KernelDisplayError::new("query grant-scoped display state", error.to_string())
    })?;
    Ok(KernelDisplayObservation {
        topology: map_topology(topology),
        grant_state: map_grant_state(grant.state),
    })
}

fn map_topology(topology: OutputTopology) -> DisplayTopology {
    DisplayTopology {
        attachment: match topology.attachment {
            MonitorAttachmentState::Attached => AttachmentState::Attached,
            MonitorAttachmentState::Detached => AttachmentState::Detached,
            MonitorAttachmentState::Unknown => AttachmentState::Unknown,
        },
        route: topology.route.map(map_route),
    }
}

fn map_route(route: ActiveOutputRoute) -> ActiveRoute {
    ActiveRoute {
        target: RouteTarget::new(route.crtc_id),
        mode: RoutedMode {
            width: route.width.get(),
            height: route.height.get(),
            refresh_millihz: route.refresh_millihz.get(),
            flags: route.mode_flags,
        },
    }
}

pub(crate) fn map_grant_state(state: CastKmsGrantState) -> DisplayGrantState {
    match state {
        CastKmsGrantState::Pending => DisplayGrantState::Pending,
        CastKmsGrantState::Active => DisplayGrantState::Active,
        CastKmsGrantState::SuspendedNoMaster => DisplayGrantState::SuspendedNoMaster,
        CastKmsGrantState::SuspendedOtherMaster => DisplayGrantState::SuspendedOtherMaster,
        CastKmsGrantState::SuspendedForeignContent => DisplayGrantState::SuspendedForeignContent,
        CastKmsGrantState::Revoked => DisplayGrantState::Revoked,
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    #[test]
    fn rejects_a_zero_poll_interval_before_constructing_tokio_interval() {
        assert!(validate_poll_interval(Duration::ZERO).is_err());
        assert!(validate_poll_interval(Duration::from_millis(1)).is_ok());
    }

    #[test]
    fn maps_kernel_route_identity_without_losing_the_capture_target() {
        let topology = map_topology(OutputTopology {
            attachment: MonitorAttachmentState::Attached,
            route: Some(ActiveOutputRoute {
                crtc_id: NonZeroU32::new(23).unwrap(),
                width: NonZeroU32::new(1920).unwrap(),
                height: NonZeroU32::new(1080).unwrap(),
                refresh_millihz: NonZeroU32::new(60_000).unwrap(),
                mode_flags: 4,
            }),
        });

        assert_eq!(topology.attachment, AttachmentState::Attached);
        let route = topology.route.unwrap();
        assert_eq!(route.target.get(), 23);
        assert_eq!(route.mode.width, 1920);
        assert_eq!(route.mode.flags, 4);
    }
}
