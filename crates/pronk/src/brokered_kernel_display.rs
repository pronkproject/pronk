//! Kernel-display observation and teardown for one brokered display session.

use std::io;
use std::num::NonZeroU32;
use std::time::Duration;

use async_trait::async_trait;
use drm_capture::Access as CaptureAccess;
use nix::libc;
use pronk_capture_broker::Session;
use pronk_core::edid::EdidMode;
use tokio::time::{interval, MissedTickBehavior};

use crate::display_state::{
    ActiveRoute, AttachmentState, DisplayGrantState, DisplayTopology, RouteTarget, RoutedMode,
};
use crate::kernel_display_port::{
    KernelDisplayError, KernelDisplayEvent, KernelDisplayMetadata, KernelDisplayObservation,
    KernelDisplayPort,
};

pub const DEFAULT_TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub struct BrokeredKernelDisplay {
    session: Option<Session>,
    capture: CaptureAccess,
    crtc_id: NonZeroU32,
    modes: Vec<EdidMode>,
    current: KernelDisplayObservation,
    poll: tokio::time::Interval,
}

impl BrokeredKernelDisplay {
    pub fn new(
        session: Session,
        capture: CaptureAccess,
        crtc_id: NonZeroU32,
        modes: Vec<EdidMode>,
        poll_interval: Duration,
    ) -> Result<Self, KernelDisplayError> {
        if modes.is_empty() || poll_interval.is_zero() {
            return Err(KernelDisplayError::new(
                "configure brokered display observation",
                "advertised modes and a nonzero poll interval are required",
            ));
        }
        let mut poll = interval(poll_interval);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        poll.reset();
        Ok(Self {
            session: Some(session),
            capture,
            crtc_id,
            modes,
            current: unavailable_observation(),
            poll,
        })
    }

    fn session(&self) -> &Session {
        self.session
            .as_ref()
            .expect("live kernel display owns its broker session")
    }

    fn observe(&self) -> Result<Observation, KernelDisplayError> {
        let client = match self.capture.open() {
            Ok(client) => client,
            Err(error) => return classify_capture_error(error, self.current),
        };
        let description = match client.describe() {
            Ok(description) => description,
            Err(error) => return classify_capture_error(error, self.current),
        };
        active_observation(
            &self.modes,
            self.crtc_id,
            description.width.get(),
            description.height.get(),
            description.refresh_millihz.get(),
            description.mode_flags,
        )
        .map(Observation::State)
    }
}

enum Observation {
    State(KernelDisplayObservation),
    Revoked,
}

fn classify_capture_error(
    error: io::Error,
    current: KernelDisplayObservation,
) -> Result<Observation, KernelDisplayError> {
    match error.raw_os_error() {
        Some(libc::ENODEV) => Ok(Observation::State(unavailable_observation())),
        Some(libc::EACCES) => Ok(Observation::State(KernelDisplayObservation {
            topology: current.topology,
            grant_state: DisplayGrantState::SuspendedForeignContent,
        })),
        Some(libc::EKEYREVOKED) | Some(libc::ECANCELED) => Ok(Observation::Revoked),
        _ => Err(KernelDisplayError::new(
            "observe brokered capture output",
            error.to_string(),
        )),
    }
}

fn unavailable_observation() -> KernelDisplayObservation {
    KernelDisplayObservation {
        topology: DisplayTopology {
            attachment: AttachmentState::Attached,
            route: None,
        },
        grant_state: DisplayGrantState::Pending,
    }
}

fn active_observation(
    modes: &[EdidMode],
    crtc_id: NonZeroU32,
    width: u32,
    height: u32,
    refresh_millihz: u32,
    mode_flags: u32,
) -> Result<KernelDisplayObservation, KernelDisplayError> {
    if !modes
        .iter()
        .any(|mode| mode.width == width && mode.height == height)
    {
        return Err(KernelDisplayError::new(
            "match brokered capture output",
            format!("active {width}x{height} output was not advertised in its EDID"),
        ));
    }
    Ok(KernelDisplayObservation {
        topology: DisplayTopology {
            attachment: AttachmentState::Attached,
            route: Some(ActiveRoute {
                target: RouteTarget::new(crtc_id),
                mode: RoutedMode {
                    width,
                    height,
                    refresh_millihz,
                    flags: mode_flags,
                },
            }),
        },
        grant_state: DisplayGrantState::Active,
    })
}

#[async_trait]
impl KernelDisplayPort for BrokeredKernelDisplay {
    fn metadata(&self) -> KernelDisplayMetadata {
        KernelDisplayMetadata {
            session_id: self.session().id(),
        }
    }

    fn initial_observation(&self) -> KernelDisplayObservation {
        self.current
    }

    async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError> {
        loop {
            self.poll.tick().await;
            match self.observe()? {
                Observation::Revoked => return Ok(KernelDisplayEvent::Revoked),
                Observation::State(observation) if observation != self.current => {
                    self.current = observation;
                    return Ok(KernelDisplayEvent::Changed(observation));
                }
                Observation::State(_) => {}
            }
        }
    }

    async fn detach(mut self: Box<Self>) -> Result<(), KernelDisplayError> {
        let session = self
            .session
            .take()
            .expect("live kernel display owns its broker session");
        let (session, detach) = tokio::task::spawn_blocking(move || {
            let result = normalize_detach(session.detach_monitor());
            (session, result)
        })
        .await
        .map_err(|error| {
            KernelDisplayError::new("join brokered monitor detach", error.to_string())
        })?;
        let release = session.release().await;
        match (detach, release) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(KernelDisplayError::new(
                "detach brokered monitor",
                error.to_string(),
            )),
            (Ok(()), Err(error)) => Err(KernelDisplayError::new(
                "release brokered display session",
                error.to_string(),
            )),
            (Err(detach), Err(release)) => Err(KernelDisplayError::new(
                "detach and release brokered display session",
                format!("detach: {detach}; release: {release}"),
            )),
        }
    }
}

fn normalize_detach(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ECANCELED) | Some(libc::EKEYREVOKED) | Some(libc::ENODEV)
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes() -> Vec<EdidMode> {
        vec![
            EdidMode::new(1920, 1080, 60_000).unwrap(),
            EdidMode::new(1280, 720, 60_000).unwrap(),
        ]
    }

    #[test]
    fn active_output_uses_the_advertised_timing() {
        let observation =
            active_observation(&modes(), NonZeroU32::new(17).unwrap(), 1280, 720, 59_940, 5)
                .unwrap();
        assert_eq!(observation.grant_state, DisplayGrantState::Active);
        let route = observation.topology.route.unwrap();
        assert_eq!(route.target.get(), 17);
        assert_eq!(route.mode.refresh_millihz, 59_940);
        assert_eq!(route.mode.flags, 5);
    }

    #[test]
    fn unadvertised_output_dimensions_are_rejected() {
        assert!(
            active_observation(&modes(), NonZeroU32::new(17).unwrap(), 1024, 768, 60_000, 0,)
                .is_err()
        );
    }

    #[test]
    fn expected_capture_states_have_bounded_meanings() {
        assert!(matches!(
            classify_capture_error(
                io::Error::from_raw_os_error(libc::ENODEV),
                unavailable_observation()
            )
            .unwrap(),
            Observation::State(KernelDisplayObservation {
                grant_state: DisplayGrantState::Pending,
                ..
            })
        ));
        assert!(matches!(
            classify_capture_error(
                io::Error::from_raw_os_error(libc::EACCES),
                unavailable_observation()
            )
            .unwrap(),
            Observation::State(KernelDisplayObservation {
                grant_state: DisplayGrantState::SuspendedForeignContent,
                ..
            })
        ));
        assert!(matches!(
            classify_capture_error(
                io::Error::from_raw_os_error(libc::EKEYREVOKED),
                unavailable_observation()
            )
            .unwrap(),
            Observation::Revoked
        ));
        assert!(classify_capture_error(
            io::Error::from_raw_os_error(libc::EIO),
            unavailable_observation()
        )
        .is_err());
    }

    #[test]
    fn teardown_accepts_authority_that_is_already_gone() {
        for errno in [libc::ECANCELED, libc::EKEYREVOKED, libc::ENODEV] {
            assert!(normalize_detach(Err(io::Error::from_raw_os_error(errno))).is_ok());
        }
        assert!(normalize_detach(Err(io::Error::from_raw_os_error(libc::EIO))).is_err());
    }
}
