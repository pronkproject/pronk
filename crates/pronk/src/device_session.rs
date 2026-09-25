//! Private-backend adapter for the application-owned Device-session port.

use std::num::NonZeroU64;

use async_trait::async_trait;
use pronk_backend_host::{
    BackendSessionError, BackendSessionEvent, BackendSessionHandle, BackendSessionMonitor,
};
use pronk_backend_protocol::{
    validate_media_configuration, DisplayMode, MediaConfiguration, MediaKind, PipeWireTarget,
    StopReason, SuspendReason,
};
use zbus::zvariant::OwnedFd as ZbusOwnedFd;

use crate::device_session_port::{
    DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaSetup, DeviceMediaStopReason,
    DeviceMediaSuspendReason, DeviceMediaTarget, DeviceSessionError, DeviceSessionEvent,
    DeviceSessionEventPort, DeviceSessionPort, DeviceSessionStopReason,
};

#[derive(Debug)]
pub struct BackendDeviceSession {
    session: Option<BackendSessionHandle>,
    media: BackendMediaLifecycle,
}

impl BackendDeviceSession {
    pub fn new(session: BackendSessionHandle) -> Self {
        Self {
            session: Some(session),
            media: BackendMediaLifecycle::Prepared {
                last_completed: None,
            },
        }
    }

    fn session(&self) -> &BackendSessionHandle {
        self.session
            .as_ref()
            .expect("live backend Device-session adapter owns its handle")
    }
}

#[derive(Debug)]
pub struct BackendDeviceSessionEvents {
    monitor: BackendSessionMonitor,
}

impl BackendDeviceSessionEvents {
    pub async fn start(session: &BackendSessionHandle) -> Result<Self, BackendSessionError> {
        Ok(Self {
            monitor: session.start_event_monitor().await?,
        })
    }
}

#[async_trait]
impl DeviceSessionEventPort for BackendDeviceSessionEvents {
    async fn next_event(&mut self) -> Option<DeviceSessionEvent> {
        self.monitor.next_event().await.map(map_session_event)
    }

    async fn shutdown(self: Box<Self>) {
        self.monitor.shutdown().await;
    }
}

fn map_session_event(event: BackendSessionEvent) -> DeviceSessionEvent {
    let (session_generation, error) = match event {
        BackendSessionEvent::Disconnected {
            session_generation,
            error_text,
        } => (
            session_generation,
            format!("Device disconnected: {error_text}"),
        ),
        BackendSessionEvent::FatalError {
            session_generation,
            error_text,
        } => (
            session_generation,
            format!("Device session failed: {error_text}"),
        ),
        BackendSessionEvent::ConnectionClosed { session_generation } => {
            (session_generation, "backend P2P connection closed".into())
        }
        BackendSessionEvent::MonitorFailed {
            session_generation,
            error_text,
        } => (
            session_generation,
            format!("Device-session event monitor failed: {error_text}"),
        ),
    };
    DeviceSessionEvent {
        session_generation: NonZeroU64::new(session_generation)
            .expect("validated backend session generations are nonzero"),
        error,
    }
}

#[async_trait]
impl DeviceSessionPort for BackendDeviceSession {
    async fn configure_media(&mut self, setup: DeviceMediaSetup) -> Result<(), DeviceSessionError> {
        let media_generation = setup.media_generation;
        let (remotes, targets): (Vec<_>, Vec<_>) = setup
            .endpoints
            .into_iter()
            .map(|endpoint| {
                (
                    ZbusOwnedFd::from(endpoint.remote),
                    map_media_target(endpoint.target),
                )
            })
            .unzip();
        let configuration = map_configuration(setup.configuration);
        validate_media_configuration(
            remotes.len(),
            &targets,
            &configuration,
            media_generation.get(),
        )
        .map_err(|error| DeviceSessionError::new(format!("invalid media setup: {error}")))?;

        self.media.begin_configure(media_generation)?;
        let result = self
            .session()
            .configure_media(remotes, targets, configuration, media_generation)
            .await
            .map_err(|error| DeviceSessionError::new(error.to_string()));
        if result.is_ok() {
            self.media = BackendMediaLifecycle::Configured(media_generation);
        }
        result
    }

    async fn start_media(
        &mut self,
        media_generation: NonZeroU64,
    ) -> Result<(), DeviceSessionError> {
        self.media
            .begin_transition(media_generation, BackendMediaPhase::Configured, "start")?;
        let result = self
            .session()
            .start_media(media_generation)
            .await
            .map_err(|error| DeviceSessionError::new(error.to_string()));
        if result.is_ok() {
            self.media = BackendMediaLifecycle::Streaming(media_generation);
        }
        result
    }

    async fn suspend_media(
        &mut self,
        media_generation: NonZeroU64,
        reason: DeviceMediaSuspendReason,
    ) -> Result<(), DeviceSessionError> {
        self.media
            .begin_transition(media_generation, BackendMediaPhase::Streaming, "suspend")?;
        let reason = match reason {
            DeviceMediaSuspendReason::OutputDisabled => SuspendReason::OutputDisabled,
            DeviceMediaSuspendReason::ModeChanged => SuspendReason::ModeChange,
            DeviceMediaSuspendReason::DeviceUnavailable => SuspendReason::DeviceUnavailable,
            DeviceMediaSuspendReason::SessionInactive => SuspendReason::SessionInactive,
        };
        let result = self
            .session()
            .suspend_media(reason)
            .await
            .map_err(|error| DeviceSessionError::new(error.to_string()));
        if result.is_ok() {
            self.media = BackendMediaLifecycle::Suspended(media_generation);
        }
        result
    }

    async fn resume_media(
        &mut self,
        media_generation: NonZeroU64,
    ) -> Result<(), DeviceSessionError> {
        self.media
            .begin_transition(media_generation, BackendMediaPhase::Suspended, "resume")?;
        let result = self
            .session()
            .resume_media(media_generation)
            .await
            .map_err(|error| DeviceSessionError::new(error.to_string()));
        if result.is_ok() {
            self.media = BackendMediaLifecycle::Streaming(media_generation);
        }
        result
    }

    async fn stop_media(
        &mut self,
        media_generation: NonZeroU64,
        reason: DeviceMediaStopReason,
    ) -> Result<(), DeviceSessionError> {
        if !self.media.begin_stop(media_generation)? {
            return Ok(());
        }
        let reason = match reason {
            DeviceMediaStopReason::OutputDisabled | DeviceMediaStopReason::ModeChanged => {
                StopReason::UserRequest
            }
            DeviceMediaStopReason::DisplayRemoved => StopReason::DisplayRemoved,
            DeviceMediaStopReason::BackendShutdown => StopReason::BackendShutdown,
            DeviceMediaStopReason::TransportFailure => StopReason::TransportFailure,
        };
        let result = self
            .session()
            .stop_media(media_generation, reason)
            .await
            .map_err(|error| DeviceSessionError::new(error.to_string()));
        if result.is_ok() {
            self.media = BackendMediaLifecycle::Prepared {
                last_completed: Some(media_generation),
            };
        }
        result
    }

    async fn stop(
        mut self: Box<Self>,
        reason: DeviceSessionStopReason,
    ) -> Result<(), DeviceSessionError> {
        let session = self
            .session
            .take()
            .expect("live backend Device-session adapter owns its handle");
        let reason = match reason {
            DeviceSessionStopReason::DisplayRemoved => StopReason::DisplayRemoved,
            DeviceSessionStopReason::DaemonShutdown => StopReason::BackendShutdown,
        };
        session
            .stop(reason)
            .await
            .map_err(|error| DeviceSessionError::new(error.to_string()))
    }
}

fn map_media_target(target: DeviceMediaTarget) -> PipeWireTarget {
    PipeWireTarget {
        kind: match target.kind {
            DeviceMediaKind::Video => MediaKind::Video,
            DeviceMediaKind::Audio => MediaKind::Audio,
        },
        node_name: target.node_name,
        object_serial: target.object_serial.get(),
        session_id: target.session_id,
        device_instance: target.device_instance,
        connector_id: target.connector_id.get(),
        output_index: target.output_index,
        media_generation: target.media_generation.get(),
        render_device: target.render_device.map(|device| {
            pronk_backend_protocol::RenderDeviceIdentity {
                major: device.major,
                minor: device.minor,
            }
        }),
        caps: target.caps,
    }
}

fn map_configuration(configuration: DeviceMediaConfiguration) -> MediaConfiguration {
    MediaConfiguration {
        video_profile_id: configuration.video_profile_id,
        audio_profile_id: configuration.audio_profile_id,
        mode: DisplayMode {
            width: configuration.mode.width,
            height: configuration.mode.height,
            refresh_millihz: configuration.mode.refresh_millihz,
            // RoutedMode carries the kernel's DRM timing flags so topology
            // changes remain lossless inside the core. The backend protocol
            // reserves this field, and a Device backend has no use for KMS
            // sync-polarity bits.
            flags: 0,
        },
        video_bitrate: configuration.video_bitrate.get(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendMediaLifecycle {
    Prepared {
        last_completed: Option<NonZeroU64>,
    },
    Configured(NonZeroU64),
    Streaming(NonZeroU64),
    Suspended(NonZeroU64),
    /// The peer may have observed the operation. Only generation-matched
    /// cleanup or final session teardown is safe from this state.
    Uncertain(NonZeroU64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendMediaPhase {
    Configured,
    Streaming,
    Suspended,
}

impl BackendMediaLifecycle {
    fn begin_configure(&mut self, media_generation: NonZeroU64) -> Result<(), DeviceSessionError> {
        match *self {
            Self::Prepared { last_completed }
                if last_completed.is_none_or(|completed| media_generation > completed) =>
            {
                *self = Self::Uncertain(media_generation);
                Ok(())
            }
            Self::Prepared {
                last_completed: Some(last_completed),
            } => Err(DeviceSessionError::new(format!(
                "media generation {} is not newer than completed generation {}",
                media_generation, last_completed
            ))),
            state => Err(DeviceSessionError::new(format!(
                "cannot configure media generation {} while backend media is {}",
                media_generation.get(),
                state.description()
            ))),
        }
    }

    fn begin_transition(
        &mut self,
        media_generation: NonZeroU64,
        required: BackendMediaPhase,
        operation: &'static str,
    ) -> Result<(), DeviceSessionError> {
        let matches = matches!(
            (*self, required),
            (Self::Configured(active), BackendMediaPhase::Configured)
                | (Self::Streaming(active), BackendMediaPhase::Streaming)
                | (Self::Suspended(active), BackendMediaPhase::Suspended)
                if active == media_generation
        );
        if !matches {
            return Err(DeviceSessionError::new(format!(
                "cannot {operation} media generation {} while backend media is {}",
                media_generation.get(),
                self.description()
            )));
        }
        *self = Self::Uncertain(media_generation);
        Ok(())
    }

    /// Returns false when this exact generation was already stopped.
    fn begin_stop(&mut self, media_generation: NonZeroU64) -> Result<bool, DeviceSessionError> {
        match *self {
            Self::Prepared { last_completed }
                if last_completed.is_none_or(|completed| media_generation >= completed) =>
            {
                // ConfigureMedia may reject locally before crossing the D-Bus
                // boundary. The caller deliberately treats every error as an
                // ambiguous authority transfer and follows it with StopMedia;
                // consume that generation without contacting the backend.
                *self = Self::Prepared {
                    last_completed: Some(media_generation),
                };
                Ok(false)
            }
            Self::Configured(active)
            | Self::Streaming(active)
            | Self::Suspended(active)
            | Self::Uncertain(active)
                if active == media_generation =>
            {
                *self = Self::Uncertain(media_generation);
                Ok(true)
            }
            state => Err(DeviceSessionError::new(format!(
                "cannot stop media generation {} while backend media is {}",
                media_generation.get(),
                state.description()
            ))),
        }
    }

    fn description(self) -> String {
        match self {
            Self::Prepared { last_completed } => match last_completed {
                Some(generation) => format!("prepared (last completed generation {generation})"),
                None => "prepared (no completed generation)".into(),
            },
            Self::Configured(generation) => format!("configured generation {generation}"),
            Self::Streaming(generation) => format!("streaming generation {generation}"),
            Self::Suspended(generation) => format!("suspended generation {generation}"),
            Self::Uncertain(generation) => format!("uncertain generation {generation}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::device_session_port::RenderDeviceIdentity;
    use crate::display_state::RoutedMode;

    fn generation(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    #[test]
    fn lifecycle_rejects_stale_and_overlapping_generations() {
        let mut lifecycle = BackendMediaLifecycle::Prepared {
            last_completed: None,
        };
        lifecycle.begin_configure(generation(1)).unwrap();
        assert!(lifecycle.begin_configure(generation(2)).is_err());

        lifecycle = BackendMediaLifecycle::Configured(generation(1));
        assert!(lifecycle
            .begin_transition(generation(2), BackendMediaPhase::Configured, "start")
            .is_err());
        lifecycle
            .begin_transition(generation(1), BackendMediaPhase::Configured, "start")
            .unwrap();
        assert_eq!(lifecycle, BackendMediaLifecycle::Uncertain(generation(1)));
    }

    #[test]
    fn ambiguous_operations_only_allow_matching_cleanup() {
        let mut lifecycle = BackendMediaLifecycle::Uncertain(generation(4));
        assert!(lifecycle.begin_stop(generation(3)).is_err());
        assert!(lifecycle.begin_stop(generation(4)).unwrap());
        lifecycle = BackendMediaLifecycle::Prepared {
            last_completed: Some(generation(4)),
        };
        assert!(!lifecycle.begin_stop(generation(4)).unwrap());
        assert_eq!(
            lifecycle
                .begin_configure(generation(3))
                .unwrap_err()
                .to_string(),
            "media generation 3 is not newer than completed generation 4"
        );
        assert!(lifecycle.begin_configure(generation(4)).is_err());
        lifecycle.begin_configure(generation(5)).unwrap();
    }

    #[test]
    fn locally_rejected_configuration_can_consume_its_cleanup_generation() {
        let mut lifecycle = BackendMediaLifecycle::Prepared {
            last_completed: None,
        };
        assert!(!lifecycle.begin_stop(generation(1)).unwrap());
        assert_eq!(
            lifecycle,
            BackendMediaLifecycle::Prepared {
                last_completed: Some(generation(1)),
            }
        );
        assert!(!lifecycle.begin_stop(generation(1)).unwrap());
        assert!(lifecycle.begin_configure(generation(1)).is_err());
        lifecycle.begin_configure(generation(2)).unwrap();
    }

    #[test]
    fn backend_protocol_mapping_drops_kms_timing_flags() {
        let configuration = map_configuration(DeviceMediaConfiguration {
            video_profile_id: "h264-high".into(),
            audio_profile_id: None,
            mode: RoutedMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 5,
            },
            video_bitrate: NonZeroU64::new(8_000_000).unwrap(),
        });

        assert_eq!(configuration.mode.width, 1920);
        assert_eq!(configuration.mode.height, 1080);
        assert_eq!(configuration.mode.refresh_millihz, 60_000);
        assert_eq!(configuration.mode.flags, 0);
    }

    #[test]
    fn backend_protocol_mapping_preserves_the_render_device() {
        let target = map_media_target(DeviceMediaTarget {
            kind: DeviceMediaKind::Video,
            node_name: "pronk.video.test".into(),
            object_serial: generation(11),
            session_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            device_instance: "castkms-card1".into(),
            connector_id: NonZeroU32::new(51).unwrap(),
            output_index: 0,
            media_generation: generation(7),
            render_device: Some(RenderDeviceIdentity {
                major: 226,
                minor: 128,
            }),
            caps: "video/x-raw(memory:DMABuf)".into(),
        });

        assert_eq!(
            target.render_device,
            Some(pronk_backend_protocol::RenderDeviceIdentity {
                major: 226,
                minor: 128,
            })
        );
    }
}
