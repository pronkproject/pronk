//! Generation-safe replacement of one prepared Device-session transport.
//!
//! The media driver keeps using the application-owned [`DeviceSessionPort`]
//! while a separate recovery coordinator may install a freshly prepared
//! backend session.  The shared state contains no backend, D-Bus, PipeWire, or
//! CastKMS types.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::device_control_port::{DeviceControlError, DeviceControlOperation, DeviceControlPort};
use crate::device_session_port::{
    DeviceMediaSetup, DeviceMediaStopReason, DeviceMediaSuspendReason, DeviceSessionError,
    DeviceSessionPort, DeviceSessionStopReason,
};

#[derive(Debug)]
pub struct DeviceSessionReplacement {
    pub session_generation: NonZeroU64,
    pub session: Box<dyn DeviceSessionPort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSessionRetirementReport {
    pub retired_session_generation: Option<NonZeroU64>,
    pub retired_media_generation: Option<NonZeroU64>,
    pub cleanup_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSessionInstallationReport {
    pub installed_session_generation: NonZeroU64,
}

pub struct DeviceSessionReplacementHandle {
    shared: Arc<Mutex<SharedState>>,
}

/// Proof that the previous Device session has completed its final teardown
/// attempt and the replacement slot is vacant.
///
/// Only this permit can install a new session. Keeping the sole replacement
/// handle mutably borrowed prevents a recovery path from accidentally making
/// a second backend session before retiring the first one.
pub struct DeviceSessionInstallationPermit<'a> {
    replacement: &'a mut DeviceSessionReplacementHandle,
    retirement: DeviceSessionRetirementReport,
}

impl fmt::Debug for DeviceSessionReplacementHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceSessionReplacementHandle")
            .finish_non_exhaustive()
    }
}

impl DeviceSessionReplacementHandle {
    /// Retire the active transport before creating its replacement.
    ///
    /// Some backends intentionally allow only one session per Device. The
    /// returned permit makes that break-before-make ordering explicit while
    /// preserving stale-safe media cleanup through the shared facade.
    pub async fn retire_current(
        &mut self,
    ) -> Result<DeviceSessionInstallationPermit<'_>, DeviceSessionReplacementError> {
        let retired = {
            let mut shared = self.shared.lock().await;
            if shared.closed {
                return Err(DeviceSessionReplacementError::Stopped);
            }
            let retired = shared.current.take();
            if let Some(media_generation) = retired
                .as_ref()
                .and_then(|session| session.media_generation)
            {
                shared.retired_media_generations.insert(media_generation);
            }
            retired
        };
        let retired_session_generation = retired.as_ref().map(|session| session.session_generation);
        let retired_media_generation = retired
            .as_ref()
            .and_then(|session| session.media_generation);
        let cleanup_error = match retired {
            Some(retired) => retired
                .session
                .stop(DeviceSessionStopReason::DaemonShutdown)
                .await
                .err()
                .map(|error| error.to_string()),
            None => None,
        };
        Ok(DeviceSessionInstallationPermit {
            replacement: self,
            retirement: DeviceSessionRetirementReport {
                retired_session_generation,
                retired_media_generation,
                cleanup_error,
            },
        })
    }

    async fn install(
        &self,
        replacement: DeviceSessionReplacement,
    ) -> Result<DeviceSessionInstallationReport, DeviceSessionReplacementError> {
        let DeviceSessionReplacement {
            session_generation,
            session,
        } = replacement;
        let mut replacement_session = Some(session);
        let mut rejected = None;
        let installed = {
            let mut shared = self.shared.lock().await;
            if shared.closed {
                rejected = Some(DeviceSessionReplacementError::Stopped);
                None
            } else if let Some(current) = shared.current.as_ref() {
                rejected = Some(DeviceSessionReplacementError::Occupied {
                    current: current.session_generation,
                });
                None
            } else if session_generation <= shared.last_session_generation {
                rejected = Some(DeviceSessionReplacementError::StaleGeneration {
                    current: shared.last_session_generation,
                    replacement: session_generation,
                });
                None
            } else {
                shared.current = Some(ActiveSession {
                    session_generation,
                    media_generation: None,
                    session: replacement_session
                        .take()
                        .expect("accepted replacement still owns its session"),
                });
                shared.last_session_generation = session_generation;
                Some(())
            }
        };

        let Some(()) = installed else {
            // A session that was created for a stale or stopped owner must not
            // be dropped without its final protocol teardown attempt.
            let cleanup_error = replacement_session
                .take()
                .expect("rejected replacement still owns its session")
                .stop(DeviceSessionStopReason::DaemonShutdown)
                .await
                .err()
                .map(|error| error.to_string());
            return Err(
                match (
                    rejected.expect("rejected replacement has a reason"),
                    cleanup_error,
                ) {
                    (DeviceSessionReplacementError::Stopped, Some(cleanup)) => {
                        DeviceSessionReplacementError::RejectedCleanup {
                            reason: "Device-session owner has stopped".into(),
                            cleanup,
                        }
                    }
                    (
                        DeviceSessionReplacementError::StaleGeneration {
                            current,
                            replacement,
                        },
                        Some(cleanup),
                    ) => DeviceSessionReplacementError::RejectedCleanup {
                        reason: format!(
                        "replacement session generation {replacement} is not newer than {current}"
                    ),
                        cleanup,
                    },
                    (DeviceSessionReplacementError::Occupied { current }, Some(cleanup)) => {
                        DeviceSessionReplacementError::RejectedCleanup {
                            reason: format!(
                                "Device-session generation {current} still occupies the replacement slot"
                            ),
                            cleanup,
                        }
                    }
                    (error, None) => error,
                    (DeviceSessionReplacementError::RejectedCleanup { .. }, _) => {
                        unreachable!("a rejected-cleanup error is never an initial reason")
                    }
                },
            );
        };

        Ok(DeviceSessionInstallationReport {
            installed_session_generation: session_generation,
        })
    }
}

impl DeviceSessionInstallationPermit<'_> {
    pub fn retirement(&self) -> &DeviceSessionRetirementReport {
        &self.retirement
    }

    pub async fn install(
        self,
        replacement: DeviceSessionReplacement,
    ) -> Result<DeviceSessionInstallationReport, DeviceSessionReplacementError> {
        self.replacement.install(replacement).await
    }
}

/// Build the media-driver port, replaceable control facade, and separate
/// replacement capability over one prepared Device session.
pub fn replaceable_device_session(
    initial_session_generation: NonZeroU64,
    initial_session: Box<dyn DeviceSessionPort>,
) -> (
    Box<dyn DeviceSessionPort>,
    Arc<dyn DeviceControlPort>,
    DeviceSessionReplacementHandle,
) {
    let shared = Arc::new(Mutex::new(SharedState {
        current: Some(ActiveSession {
            session_generation: initial_session_generation,
            media_generation: None,
            session: initial_session,
        }),
        last_session_generation: initial_session_generation,
        retired_media_generations: BTreeSet::new(),
        closed: false,
    }));
    (
        Box::new(ReplaceableDeviceSessionPort {
            shared: Arc::clone(&shared),
        }),
        Arc::new(ReplaceableDeviceControlPort {
            shared: Arc::clone(&shared),
        }),
        DeviceSessionReplacementHandle { shared },
    )
}

struct ActiveSession {
    session_generation: NonZeroU64,
    media_generation: Option<NonZeroU64>,
    session: Box<dyn DeviceSessionPort>,
}

impl fmt::Debug for ActiveSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveSession")
            .field("session_generation", &self.session_generation)
            .field("media_generation", &self.media_generation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct SharedState {
    current: Option<ActiveSession>,
    last_session_generation: NonZeroU64,
    retired_media_generations: BTreeSet<NonZeroU64>,
    closed: bool,
}

struct ReplaceableDeviceSessionPort {
    shared: Arc<Mutex<SharedState>>,
}

struct ReplaceableDeviceControlPort {
    shared: Arc<Mutex<SharedState>>,
}

impl fmt::Debug for ReplaceableDeviceControlPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplaceableDeviceControlPort")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DeviceControlPort for ReplaceableDeviceControlPort {
    async fn transmit_control(
        &self,
        operation: DeviceControlOperation,
    ) -> Result<(), DeviceControlError> {
        let mut shared = self.shared.lock().await;
        let current = live_session(&mut shared)
            .map_err(|error| DeviceControlError::new(error.to_string()))?;
        current.session.transmit_control(operation).await
    }
}

impl fmt::Debug for ReplaceableDeviceSessionPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplaceableDeviceSessionPort")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DeviceSessionPort for ReplaceableDeviceSessionPort {
    async fn configure_media(&mut self, setup: DeviceMediaSetup) -> Result<(), DeviceSessionError> {
        let generation = setup.media_generation;
        let mut shared = self.shared.lock().await;
        let current = live_session(&mut shared)?;
        if let Some(active) = current.media_generation {
            return Err(DeviceSessionError::new(format!(
                "cannot configure media generation {generation} while generation {active} is active"
            )));
        }
        // ConfigureMedia transfers descriptors and is ambiguous on
        // interruption.  Remember the generation before crossing the port.
        current.media_generation = Some(generation);
        current.session.configure_media(setup).await
    }

    async fn start_media(
        &mut self,
        media_generation: NonZeroU64,
    ) -> Result<(), DeviceSessionError> {
        let mut shared = self.shared.lock().await;
        let current = matching_session(&mut shared, media_generation, "start")?;
        current.session.start_media(media_generation).await
    }

    async fn suspend_media(
        &mut self,
        media_generation: NonZeroU64,
        reason: DeviceMediaSuspendReason,
    ) -> Result<(), DeviceSessionError> {
        let mut shared = self.shared.lock().await;
        let current = matching_session(&mut shared, media_generation, "suspend")?;
        current
            .session
            .suspend_media(media_generation, reason)
            .await
    }

    async fn resume_media(
        &mut self,
        media_generation: NonZeroU64,
    ) -> Result<(), DeviceSessionError> {
        let mut shared = self.shared.lock().await;
        let current = matching_session(&mut shared, media_generation, "resume")?;
        current.session.resume_media(media_generation).await
    }

    async fn stop_media(
        &mut self,
        media_generation: NonZeroU64,
        reason: DeviceMediaStopReason,
    ) -> Result<(), DeviceSessionError> {
        let mut shared = self.shared.lock().await;
        if shared.retired_media_generations.remove(&media_generation) {
            // Replacing a whole transport definitively retired its authority;
            // this completes the media driver's matching cleanup locally.
            return Ok(());
        }
        let current = live_session(&mut shared)?;
        match current.media_generation {
            None => return Ok(()),
            Some(active) if active != media_generation => {
                return Err(DeviceSessionError::new(format!(
                    "cannot stop media generation {media_generation}; active generation is {active}"
                )))
            }
            Some(_) => {}
        }
        current.session.stop_media(media_generation, reason).await?;
        current.media_generation = None;
        Ok(())
    }

    async fn stop(
        self: Box<Self>,
        reason: DeviceSessionStopReason,
    ) -> Result<(), DeviceSessionError> {
        let current = {
            let mut shared = self.shared.lock().await;
            if shared.closed {
                return Ok(());
            }
            shared.closed = true;
            shared.retired_media_generations.clear();
            shared.current.take()
        };
        match current {
            Some(current) => current.session.stop(reason).await,
            None => Ok(()),
        }
    }
}

fn live_session(shared: &mut SharedState) -> Result<&mut ActiveSession, DeviceSessionError> {
    if shared.closed {
        return Err(DeviceSessionError::new("Device-session owner has stopped"));
    }
    shared
        .current
        .as_mut()
        .ok_or_else(|| DeviceSessionError::new("no prepared Device session is installed"))
}

fn matching_session<'a>(
    shared: &'a mut SharedState,
    media_generation: NonZeroU64,
    operation: &'static str,
) -> Result<&'a mut ActiveSession, DeviceSessionError> {
    let current = live_session(shared)?;
    if current.media_generation != Some(media_generation) {
        return Err(DeviceSessionError::new(format!(
            "cannot {operation} media generation {media_generation}; active generation is {:?}",
            current.media_generation
        )));
    }
    Ok(current)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeviceSessionReplacementError {
    #[error("Device-session owner has stopped")]
    Stopped,
    #[error("Device-session generation {current} still occupies the replacement slot")]
    Occupied { current: NonZeroU64 },
    #[error(
        "replacement session generation {replacement} is not newer than current generation {current}"
    )]
    StaleGeneration {
        current: NonZeroU64,
        replacement: NonZeroU64,
    },
    #[error("{reason}; rejected session cleanup also failed: {cleanup}")]
    RejectedCleanup { reason: String, cleanup: String },
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;
    use crate::device_session_port::{DeviceMediaConfiguration, DeviceMediaEndpoint};
    use crate::display_state::RoutedMode;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Control(&'static str, DeviceControlOperation),
        Configure(&'static str, u64),
        Start(&'static str, u64),
        StopMedia(&'static str, u64),
        Stop(&'static str, DeviceSessionStopReason),
    }

    #[derive(Debug)]
    struct FakeSession {
        name: &'static str,
        calls: Arc<StdMutex<Vec<Call>>>,
    }

    #[async_trait]
    impl DeviceSessionPort for FakeSession {
        async fn transmit_control(
            &mut self,
            operation: DeviceControlOperation,
        ) -> Result<(), DeviceControlError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Control(self.name, operation));
            Ok(())
        }

        async fn configure_media(
            &mut self,
            setup: DeviceMediaSetup,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Configure(self.name, setup.media_generation.get()));
            Ok(())
        }

        async fn start_media(
            &mut self,
            media_generation: NonZeroU64,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Start(self.name, media_generation.get()));
            Ok(())
        }

        async fn suspend_media(
            &mut self,
            _media_generation: NonZeroU64,
            _reason: DeviceMediaSuspendReason,
        ) -> Result<(), DeviceSessionError> {
            Ok(())
        }

        async fn resume_media(
            &mut self,
            _media_generation: NonZeroU64,
        ) -> Result<(), DeviceSessionError> {
            Ok(())
        }

        async fn stop_media(
            &mut self,
            media_generation: NonZeroU64,
            _reason: DeviceMediaStopReason,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::StopMedia(self.name, media_generation.get()));
            Ok(())
        }

        async fn stop(
            self: Box<Self>,
            reason: DeviceSessionStopReason,
        ) -> Result<(), DeviceSessionError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Stop(self.name, reason));
            Ok(())
        }
    }

    fn generation(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn session(name: &'static str, calls: &Arc<StdMutex<Vec<Call>>>) -> Box<dyn DeviceSessionPort> {
        Box::new(FakeSession {
            name,
            calls: Arc::clone(calls),
        })
    }

    fn empty_setup(generation: u64) -> DeviceMediaSetup {
        DeviceMediaSetup {
            media_generation: NonZeroU64::new(generation).unwrap(),
            endpoints: Vec::<DeviceMediaEndpoint>::new(),
            configuration: DeviceMediaConfiguration {
                video_profile_id: "test".into(),
                audio_profile_id: None,
                mode: RoutedMode {
                    width: 640,
                    height: 480,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
                video_bitrate: NonZeroU64::new(1).unwrap(),
            },
        }
    }

    #[tokio::test]
    async fn replacement_retires_old_authority_and_accepts_a_fresh_generation() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (mut port, control, mut replacement) =
            replaceable_device_session(generation(1), session("old", &calls));
        control
            .transmit_control(DeviceControlOperation::simple(
                crate::device_control_port::DeviceControlKind::Activate,
            ))
            .await
            .unwrap();
        port.configure_media(empty_setup(4)).await.unwrap();
        port.start_media(generation(4)).await.unwrap();

        let installation = replacement.retire_current().await.unwrap();
        assert_eq!(
            installation.retirement(),
            &DeviceSessionRetirementReport {
                retired_session_generation: Some(generation(1)),
                retired_media_generation: Some(generation(4)),
                cleanup_error: None,
            }
        );
        let report = installation
            .install(DeviceSessionReplacement {
                session_generation: generation(2),
                session: session("new", &calls),
            })
            .await
            .unwrap();
        assert_eq!(report.installed_session_generation, generation(2));
        control
            .transmit_control(DeviceControlOperation::valued(
                crate::device_control_port::DeviceControlKind::Volume,
                "relative",
                5,
            ))
            .await
            .unwrap();

        // The media actor may finish cleanup only after replacement.  It must
        // not send the old generation to the fresh backend session.
        port.stop_media(generation(4), DeviceMediaStopReason::TransportFailure)
            .await
            .unwrap();
        port.configure_media(empty_setup(5)).await.unwrap();
        port.start_media(generation(5)).await.unwrap();
        port.stop_media(generation(5), DeviceMediaStopReason::DisplayRemoved)
            .await
            .unwrap();
        port.stop(DeviceSessionStopReason::DisplayRemoved)
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Control(
                    "old",
                    DeviceControlOperation::simple(
                        crate::device_control_port::DeviceControlKind::Activate,
                    ),
                ),
                Call::Configure("old", 4),
                Call::Start("old", 4),
                Call::Stop("old", DeviceSessionStopReason::DaemonShutdown),
                Call::Control(
                    "new",
                    DeviceControlOperation::valued(
                        crate::device_control_port::DeviceControlKind::Volume,
                        "relative",
                        5,
                    ),
                ),
                Call::Configure("new", 5),
                Call::Start("new", 5),
                Call::StopMedia("new", 5),
                Call::Stop("new", DeviceSessionStopReason::DisplayRemoved),
            ]
        );
    }

    #[tokio::test]
    async fn stale_replacement_is_rejected_and_cleaned_up() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, _control, mut replacement) =
            replaceable_device_session(generation(3), session("current", &calls));
        let installation = replacement.retire_current().await.unwrap();
        assert!(matches!(
            installation
                .install(DeviceSessionReplacement {
                    session_generation: generation(3),
                    session: session("stale", &calls),
                })
                .await,
            Err(DeviceSessionReplacementError::StaleGeneration { .. })
        ));
        port.stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Stop("current", DeviceSessionStopReason::DaemonShutdown),
                Call::Stop("stale", DeviceSessionStopReason::DaemonShutdown),
            ]
        );
    }

    #[tokio::test]
    async fn installation_refuses_to_overlap_an_active_session() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, _control, replacement) =
            replaceable_device_session(generation(1), session("current", &calls));

        assert!(matches!(
            replacement
                .install(DeviceSessionReplacement {
                    session_generation: generation(2),
                    session: session("rejected", &calls),
                })
                .await,
            Err(DeviceSessionReplacementError::Occupied { current })
                if current == generation(1)
        ));
        port.stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Stop("rejected", DeviceSessionStopReason::DaemonShutdown),
                Call::Stop("current", DeviceSessionStopReason::DaemonShutdown),
            ]
        );
    }
}
