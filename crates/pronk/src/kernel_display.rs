//! Kernel-display observation and teardown for one authorized display session.

use std::io;
use std::num::NonZeroU32;
use std::time::Duration;

use async_trait::async_trait;
use drm_capture::Access as CaptureAccess;
use nix::libc;
use pronk_core::edid::{EdidMode, ValidatedEdid};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::display_state::{
    ActiveRoute, AttachmentState, DisplayGrantState, DisplayTopology, RouteTarget, RoutedMode,
};
use crate::kernel_display_port::{
    KernelDisplayError, KernelDisplayEvent, KernelDisplayMetadata, KernelDisplayObservation,
    KernelDisplayPort,
};
use crate::kernel_session::KernelSession;
use crate::renderer_session::RendererAccess;

pub const DEFAULT_TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub struct KernelDisplayConfig {
    pub crtc_id: NonZeroU32,
    pub modes: Vec<EdidMode>,
    pub poll_interval: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("monitor attachment was cancelled")]
    Cancelled,
    #[error("prepare display observation: {0}")]
    Configuration(#[source] KernelDisplayError),
    #[error("attach monitor: {0}")]
    Rejected(#[source] io::Error),
    #[error("monitor attachment worker failed: {0}")]
    Worker(#[source] tokio::task::JoinError),
}

#[derive(Debug)]
pub struct KernelDisplay {
    session: Option<KernelSession>,
    capture: CaptureAccess,
    crtc_id: NonZeroU32,
    modes: Vec<EdidMode>,
    current: KernelDisplayObservation,
    poll: tokio::time::Interval,
}

impl KernelDisplay {
    fn prepare(
        session: KernelSession,
        config: KernelDisplayConfig,
    ) -> Result<Self, KernelDisplayError> {
        let KernelDisplayConfig {
            crtc_id,
            modes,
            poll_interval,
        } = config;
        if modes.is_empty() || poll_interval.is_zero() {
            return Err(KernelDisplayError::new(
                "configure display observation",
                "advertised modes and a nonzero poll interval are required",
            ));
        }
        let capture = session.capture_access().map_err(|error| {
            KernelDisplayError::new("retain capture for display observation", error.to_string())
        })?;
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

    /// Attach a monitor while retaining responsibility for a late completion.
    ///
    /// Observation is configured before the monitor changes. Queued work checks
    /// cancellation again before issuing the monitor operation. Once a blocking
    /// attach has started, cancellation joins it and retires any attached
    /// monitor before returning. Dropping the whole future instead relies on
    /// the session owner's release behavior when the worker finishes.
    pub async fn attach(
        session: KernelSession,
        edid: Option<ValidatedEdid>,
        config: KernelDisplayConfig,
        cancellation: CancellationToken,
    ) -> Result<Self, AttachError> {
        if cancellation.is_cancelled() {
            if let Err(error) = session.release().await {
                tracing::warn!(%error, "cancelled attachment could not release its unused session");
            }
            return Err(AttachError::Cancelled);
        }
        let display = Self::prepare(session, config).map_err(AttachError::Configuration)?;
        let before_attach = cancellation.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            let result = (!before_attach.is_cancelled()).then(|| {
                display
                    .session()
                    .attach_monitor(edid.as_ref().map(ValidatedEdid::as_bytes))
            });
            (display, result)
        });
        let joined = tokio::select! {
            biased;
            _ = cancellation.cancelled() => (&mut task).await,
            joined = &mut task => joined,
        };
        let (mut display, result) = joined.map_err(AttachError::Worker)?;
        match result {
            Some(Ok(())) if !cancellation.is_cancelled() => Ok(display),
            Some(Ok(())) => {
                if let Err(error) = Box::new(display).detach().await {
                    tracing::warn!(%error, "cancelled attachment could not retire its monitor");
                }
                Err(AttachError::Cancelled)
            }
            result => {
                let session = display
                    .session
                    .take()
                    .expect("prepared display owns its session");
                if let Err(error) = session.release().await {
                    tracing::warn!(%error, "unsuccessful attachment could not release its session");
                }
                match result {
                    Some(Err(error)) if !cancellation.is_cancelled() => {
                        Err(AttachError::Rejected(error))
                    }
                    _ => Err(AttachError::Cancelled),
                }
            }
        }
    }

    fn session(&self) -> &KernelSession {
        self.session
            .as_ref()
            .expect("live kernel display owns its display session")
    }

    /// Transfer media authority without transferring the display lifetime.
    pub fn take_renderer_access(&mut self) -> io::Result<RendererAccess> {
        self.session
            .as_mut()
            .expect("live kernel display owns its session")
            .take_renderer_access()
    }

    /// Retain final-image capture without transferring monitor or renderer control.
    pub fn capture_access(&self) -> io::Result<CaptureAccess> {
        self.capture.try_clone()
    }

    fn observe(&self) -> Result<Observation, KernelDisplayError> {
        let description = match self.capture.describe() {
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
            "observe capture output",
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
            "match capture output",
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
impl KernelDisplayPort for KernelDisplay {
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
            .expect("live kernel display owns its display session");
        let (session, detach) = tokio::task::spawn_blocking(move || {
            let result = normalize_detach(session.detach_monitor());
            (session, result)
        })
        .await
        .map_err(|error| KernelDisplayError::new("join monitor detach", error.to_string()))?;
        let release = session.release().await;
        match (detach, release) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => {
                Err(KernelDisplayError::new("detach monitor", error.to_string()))
            }
            (Ok(()), Err(error)) => Err(KernelDisplayError::new(
                "release authorized display session",
                error.to_string(),
            )),
            (Err(detach), Err(release)) => Err(KernelDisplayError::new(
                "detach and release authorized display session",
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
