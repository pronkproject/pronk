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
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

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

const RETIREMENT_STOP_TIMEOUT: Duration = Duration::from_secs(1);

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
        let mut shared = self.shared.lock().await;
        if let SessionSlot::Active(_) = shared.slot {
            let SessionSlot::Active(retired) =
                std::mem::replace(&mut shared.slot, SessionSlot::Vacant)
            else {
                unreachable!("active Device session replaced above");
            };
            if let Some(media_generation) = retired.media_generation {
                shared.retired_media_generations.insert(media_generation);
            }
            shared.slot = SessionSlot::Retiring(RetiringSession::new(retired));
        }
        let (retired_session_generation, retired_media_generation, cleanup_error) =
            match &mut shared.slot {
                SessionSlot::Retiring(retiring) => {
                    let cleanup_error = (&mut retiring.task)
                        .await
                        .unwrap_or_else(|error| {
                            Err(format!("join retired Device-session stop: {error}"))
                        })
                        .err();
                    (
                        Some(retiring.session_generation),
                        retiring.media_generation,
                        cleanup_error,
                    )
                }
                SessionSlot::Vacant => (None, None, None),
                SessionSlot::Closed => return Err(DeviceSessionReplacementError::Stopped),
                SessionSlot::Active(_) => unreachable!("active session entered retirement above"),
            };
        shared.slot = SessionSlot::Vacant;
        drop(shared);
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
        let decision = {
            let mut shared = self.shared.lock().await;
            match shared.validate_install(session_generation) {
                Ok(()) => {
                    shared.slot = SessionSlot::Active(ActiveSession {
                        session_generation,
                        media_generation: None,
                        session,
                    });
                    shared.last_session_generation = session_generation;
                    InstallationDecision::Installed(DeviceSessionInstallationReport {
                        installed_session_generation: session_generation,
                    })
                }
                Err(reason) => InstallationDecision::Rejected { reason, session },
            }
        };

        match decision {
            InstallationDecision::Installed(report) => Ok(report),
            InstallationDecision::Rejected { reason, session } => {
                // A rejected backend session still needs a final protocol
                // teardown attempt before its owner can discard it.
                let cleanup_error = spawn_final_stop(
                    session,
                    DeviceSessionStopReason::DaemonShutdown,
                    "rejected Device-session",
                )
                .await
                .unwrap_or_else(|error| Err(format!("join rejected Device-session stop: {error}")))
                .err();
                Err(reason.into_error(cleanup_error))
            }
        }
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

/// Build the media-driver port and replacement capability over one prepared
/// Device session.
pub fn replaceable_device_session(
    initial_session_generation: NonZeroU64,
    initial_session: Box<dyn DeviceSessionPort>,
) -> (Box<dyn DeviceSessionPort>, DeviceSessionReplacementHandle) {
    let shared = Arc::new(Mutex::new(SharedState {
        slot: SessionSlot::Active(ActiveSession {
            session_generation: initial_session_generation,
            media_generation: None,
            session: initial_session,
        }),
        last_session_generation: initial_session_generation,
        retired_media_generations: BTreeSet::new(),
    }));
    (
        Box::new(ReplaceableDeviceSessionPort {
            shared: Arc::clone(&shared),
        }),
        DeviceSessionReplacementHandle { shared },
    )
}

enum InstallationDecision {
    Installed(DeviceSessionInstallationReport),
    Rejected {
        reason: InstallRejection,
        session: Box<dyn DeviceSessionPort>,
    },
}

enum InstallRejection {
    Stopped,
    Occupied {
        current: NonZeroU64,
    },
    StaleGeneration {
        current: NonZeroU64,
        replacement: NonZeroU64,
    },
}

impl InstallRejection {
    fn into_error(self, cleanup_error: Option<String>) -> DeviceSessionReplacementError {
        if let Some(cleanup) = cleanup_error {
            let reason = match self {
                Self::Stopped => "Device-session owner has stopped".into(),
                Self::Occupied { current } => format!(
                    "Device-session generation {current} still occupies the replacement slot"
                ),
                Self::StaleGeneration {
                    current,
                    replacement,
                } => format!(
                    "replacement session generation {replacement} is not newer than {current}"
                ),
            };
            return DeviceSessionReplacementError::RejectedCleanup { reason, cleanup };
        }
        match self {
            Self::Stopped => DeviceSessionReplacementError::Stopped,
            Self::Occupied { current } => DeviceSessionReplacementError::Occupied { current },
            Self::StaleGeneration {
                current,
                replacement,
            } => DeviceSessionReplacementError::StaleGeneration {
                current,
                replacement,
            },
        }
    }
}

struct ActiveSession {
    session_generation: NonZeroU64,
    media_generation: Option<NonZeroU64>,
    session: Box<dyn DeviceSessionPort>,
}

#[derive(Debug)]
struct RetiringSession {
    session_generation: NonZeroU64,
    media_generation: Option<NonZeroU64>,
    task: JoinHandle<Result<(), String>>,
}

impl RetiringSession {
    fn new(active: ActiveSession) -> Self {
        let ActiveSession {
            session_generation,
            media_generation,
            session,
        } = active;
        let task = spawn_final_stop(
            session,
            DeviceSessionStopReason::DaemonShutdown,
            "retired Device-session",
        );
        Self {
            session_generation,
            media_generation,
            task,
        }
    }
}

fn spawn_final_stop(
    session: Box<dyn DeviceSessionPort>,
    reason: DeviceSessionStopReason,
    label: &'static str,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        tokio::time::timeout(RETIREMENT_STOP_TIMEOUT, session.stop(reason))
            .await
            .map_err(|_| format!("{label} stop timed out"))?
            .map_err(|error| error.to_string())
    })
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
    slot: SessionSlot,
    last_session_generation: NonZeroU64,
    retired_media_generations: BTreeSet<NonZeroU64>,
}

#[derive(Debug)]
enum SessionSlot {
    Active(ActiveSession),
    Retiring(RetiringSession),
    Vacant,
    Closed,
}

impl SharedState {
    fn validate_install(&self, generation: NonZeroU64) -> Result<(), InstallRejection> {
        match &self.slot {
            SessionSlot::Closed => return Err(InstallRejection::Stopped),
            SessionSlot::Active(current) => {
                return Err(InstallRejection::Occupied {
                    current: current.session_generation,
                });
            }
            SessionSlot::Retiring(current) => {
                return Err(InstallRejection::Occupied {
                    current: current.session_generation,
                });
            }
            SessionSlot::Vacant => {}
        }
        if generation <= self.last_session_generation {
            return Err(InstallRejection::StaleGeneration {
                current: self.last_session_generation,
                replacement: generation,
            });
        }
        Ok(())
    }
}

struct ReplaceableDeviceSessionPort {
    shared: Arc<Mutex<SharedState>>,
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
            shared.retired_media_generations.clear();
            std::mem::replace(&mut shared.slot, SessionSlot::Closed)
        };
        match current {
            SessionSlot::Active(current) => {
                spawn_final_stop(current.session, reason, "final Device-session")
                    .await
                    .unwrap_or_else(|error| Err(format!("join final Device-session stop: {error}")))
                    .map_err(DeviceSessionError::new)
            }
            SessionSlot::Retiring(retiring) => retiring
                .task
                .await
                .map_err(|error| {
                    DeviceSessionError::new(format!("join retired Device-session stop: {error}"))
                })?
                .map_err(DeviceSessionError::new),
            SessionSlot::Vacant | SessionSlot::Closed => Ok(()),
        }
    }
}

fn live_session(shared: &mut SharedState) -> Result<&mut ActiveSession, DeviceSessionError> {
    match &mut shared.slot {
        SessionSlot::Active(session) => Ok(session),
        SessionSlot::Vacant => Err(DeviceSessionError::new(
            "no prepared Device session is installed",
        )),
        SessionSlot::Retiring(_) => Err(DeviceSessionError::new(
            "Device-session retirement is in progress",
        )),
        SessionSlot::Closed => Err(DeviceSessionError::new("Device-session owner has stopped")),
    }
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
        Configure(&'static str, u64),
        Start(&'static str, u64),
        StopMedia(&'static str, u64),
        Stop(&'static str, DeviceSessionStopReason),
    }

    #[derive(Debug)]
    struct FakeSession {
        name: &'static str,
        calls: Arc<StdMutex<Vec<Call>>>,
        fail_stop: bool,
        stop_gate: Option<tokio::sync::oneshot::Receiver<()>>,
        stop_completed: Option<tokio::sync::oneshot::Sender<()>>,
    }

    #[async_trait]
    impl DeviceSessionPort for FakeSession {
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
            if let Some(gate) = self.stop_gate {
                let _ = gate.await;
            }
            if let Some(done) = self.stop_completed {
                let _ = done.send(());
            }
            if self.fail_stop {
                Err(DeviceSessionError::new("stop failed"))
            } else {
                Ok(())
            }
        }
    }

    fn generation(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn session(name: &'static str, calls: &Arc<StdMutex<Vec<Call>>>) -> Box<dyn DeviceSessionPort> {
        Box::new(FakeSession {
            name,
            calls: Arc::clone(calls),
            fail_stop: false,
            stop_gate: None,
            stop_completed: None,
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
    async fn cancelled_retirement_keeps_the_stop_owner_until_installation() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (release_stop, stop_gate) = tokio::sync::oneshot::channel();
        let initial = Box::new(FakeSession {
            name: "old",
            calls: Arc::clone(&calls),
            fail_stop: false,
            stop_gate: Some(stop_gate),
            stop_completed: None,
        });
        let (port, mut replacement) = replaceable_device_session(generation(1), initial);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), replacement.retire_current(),)
                .await
                .is_err()
        );
        assert!(matches!(
            &replacement.shared.lock().await.slot,
            SessionSlot::Retiring(_)
        ));

        release_stop.send(()).unwrap();
        let permit = replacement.retire_current().await.unwrap();
        assert_eq!(
            permit.retirement().retired_session_generation,
            Some(generation(1))
        );
        assert_eq!(permit.retirement().cleanup_error, None);
        permit
            .install(DeviceSessionReplacement {
                session_generation: generation(2),
                session: session("new", &calls),
            })
            .await
            .unwrap();
        port.stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Stop("old", DeviceSessionStopReason::DaemonShutdown),
                Call::Stop("new", DeviceSessionStopReason::DaemonShutdown),
            ]
        );
    }

    #[tokio::test]
    async fn replacement_retires_old_authority_and_accepts_a_fresh_generation() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (mut port, mut replacement) =
            replaceable_device_session(generation(1), session("old", &calls));
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
                Call::Configure("old", 4),
                Call::Start("old", 4),
                Call::Stop("old", DeviceSessionStopReason::DaemonShutdown),
                Call::Configure("new", 5),
                Call::Start("new", 5),
                Call::StopMedia("new", 5),
                Call::Stop("new", DeviceSessionStopReason::DisplayRemoved),
            ]
        );
    }

    #[tokio::test]
    async fn stopped_owner_rejects_an_outstanding_installation_permit() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, mut replacement) =
            replaceable_device_session(generation(1), session("old", &calls));
        let permit = replacement.retire_current().await.unwrap();
        port.stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .unwrap();

        let error = permit
            .install(DeviceSessionReplacement {
                session_generation: generation(2),
                session: session("new", &calls),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DeviceSessionReplacementError::Stopped));
        assert!(matches!(
            replacement.retire_current().await,
            Err(DeviceSessionReplacementError::Stopped)
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Stop("old", DeviceSessionStopReason::DaemonShutdown),
                Call::Stop("new", DeviceSessionStopReason::DaemonShutdown),
            ]
        );
    }

    #[tokio::test]
    async fn stale_replacement_is_rejected_and_cleaned_up() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, mut replacement) =
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
    async fn rejected_session_cleanup_failure_keeps_the_rejection_reason() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, mut replacement) =
            replaceable_device_session(generation(3), session("current", &calls));
        let installation = replacement.retire_current().await.unwrap();
        let error = installation
            .install(DeviceSessionReplacement {
                session_generation: generation(3),
                session: Box::new(FakeSession {
                    name: "stale",
                    calls: Arc::clone(&calls),
                    fail_stop: true,
                    stop_gate: None,
                    stop_completed: None,
                }),
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            DeviceSessionReplacementError::RejectedCleanup {
                reason: "replacement session generation 3 is not newer than 3".into(),
                cleanup: "stop failed".into(),
            }
        );
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
    async fn cancelled_rejection_keeps_candidate_cleanup_running() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, mut replacement) =
            replaceable_device_session(generation(3), session("current", &calls));
        let permit = replacement.retire_current().await.unwrap();
        let (release_stop, stop_gate) = tokio::sync::oneshot::channel();
        let (stop_completed, cleanup_done) = tokio::sync::oneshot::channel();
        let rejected = Box::new(FakeSession {
            name: "stale",
            calls: Arc::clone(&calls),
            fail_stop: false,
            stop_gate: Some(stop_gate),
            stop_completed: Some(stop_completed),
        });

        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            permit.install(DeviceSessionReplacement {
                session_generation: generation(3),
                session: rejected,
            }),
        )
        .await
        .is_err());
        release_stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), cleanup_done)
            .await
            .unwrap()
            .unwrap();
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
    async fn cancelled_final_stop_keeps_session_cleanup_running() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (release_stop, stop_gate) = tokio::sync::oneshot::channel();
        let (stop_completed, cleanup_done) = tokio::sync::oneshot::channel();
        let initial = Box::new(FakeSession {
            name: "current",
            calls: Arc::clone(&calls),
            fail_stop: false,
            stop_gate: Some(stop_gate),
            stop_completed: Some(stop_completed),
        });
        let (port, _replacement) = replaceable_device_session(generation(1), initial);

        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            port.stop(DeviceSessionStopReason::DaemonShutdown),
        )
        .await
        .is_err());
        release_stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), cleanup_done)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Call::Stop(
                "current",
                DeviceSessionStopReason::DaemonShutdown,
            )]
        );
    }

    #[tokio::test]
    async fn installation_refuses_to_overlap_an_active_session() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let (port, replacement) =
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
