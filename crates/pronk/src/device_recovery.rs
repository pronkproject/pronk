//! Per-display recovery coordinator for prepared Device sessions.
//!
//! This actor owns only replacement attempts and their cancellation.  The
//! media actor retains exclusive mutation of media state, while the
//! replaceable Device-session port retains exclusive ownership of the active
//! session transport.

use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pronk_dbus::DeviceInfo;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::device_session_port::{
    DeviceSessionEventPort, DeviceSessionPort, DeviceSessionStopReason,
};
use crate::preparation::PreparedCastDevice;
use crate::replaceable_device_session::{DeviceSessionReplacement, DeviceSessionReplacementHandle};

const RECOVERY_COMMAND_CAPACITY: usize = 4;
const RECOVERY_EVENT_CAPACITY: usize = 4;
const MAX_RECOVERY_ERROR_BYTES: usize = 512;

#[derive(Debug)]
pub struct PreparedDeviceSession {
    pub prepared: PreparedCastDevice,
    pub session: Box<dyn DeviceSessionPort>,
    pub events: Box<dyn DeviceSessionEventPort>,
}

/// Infrastructure factory used by the recovery use case.
///
/// A cancelled call must clean up any backend session it may have created
/// before returning [`DeviceSessionFactoryError::Cancelled`].
#[async_trait]
pub trait DeviceSessionFactoryPort: fmt::Debug + Send + 'static {
    async fn create_prepared_session(
        &mut self,
        device: DeviceInfo,
        session_generation: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<PreparedDeviceSession, DeviceSessionFactoryError>;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeviceSessionFactoryError {
    #[error("Device-session recovery was cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

impl DeviceSessionFactoryError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(bounded_text(message.into(), MAX_RECOVERY_ERROR_BYTES))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSessionRecoveryEvent {
    Ready {
        request_generation: u64,
        device: DeviceInfo,
        session_generation: NonZeroU64,
        retired_session_cleanup_error: Option<String>,
    },
    Failed {
        request_generation: u64,
        device: DeviceInfo,
        error: String,
    },
    TransportFailed {
        session_generation: NonZeroU64,
        error: String,
    },
}

pub struct DeviceSessionRecoveryActor {
    handle: DeviceSessionRecoveryHandle,
    events: mpsc::Receiver<DeviceSessionRecoveryEvent>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for DeviceSessionRecoveryActor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceSessionRecoveryActor")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct DeviceSessionRecoveryHandle {
    commands: mpsc::Sender<RecoveryCommand>,
    request_generation: Arc<AtomicU64>,
    phase_cancellation: Arc<Mutex<CancellationToken>>,
}

impl DeviceSessionRecoveryHandle {
    pub async fn recover(&self, device: DeviceInfo) -> Result<u64, DeviceSessionRecoveryError> {
        let request_generation = self
            .request_generation
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| DeviceSessionRecoveryError::GenerationExhausted)?
            .checked_add(1)
            .ok_or(DeviceSessionRecoveryError::GenerationExhausted)?;
        let cancellation = self.begin_request();
        self.commands
            .send(RecoveryCommand::Recover {
                request_generation,
                device,
                cancellation,
            })
            .await
            .map_err(|_| DeviceSessionRecoveryError::Stopped)?;
        Ok(request_generation)
    }

    pub fn cancel_phase(&self) {
        self.phase_cancellation
            .lock()
            .expect("Device-session recovery cancellation mutex poisoned")
            .cancel();
    }

    fn begin_request(&self) -> CancellationToken {
        let mut current = self
            .phase_cancellation
            .lock()
            .expect("Device-session recovery cancellation mutex poisoned");
        current.cancel();
        let cancellation = CancellationToken::new();
        *current = cancellation.clone();
        cancellation
    }
}

impl DeviceSessionRecoveryActor {
    pub fn spawn(
        factory: Box<dyn DeviceSessionFactoryPort>,
        replacement: DeviceSessionReplacementHandle,
        prepared: PreparedCastDevice,
        initial_session_generation: NonZeroU64,
        initial_events: Box<dyn DeviceSessionEventPort>,
    ) -> Result<Self, DeviceSessionRecoveryError> {
        tokio::runtime::Handle::try_current().map_err(|_| DeviceSessionRecoveryError::NoRuntime)?;
        let (commands, command_rx) = mpsc::channel(RECOVERY_COMMAND_CAPACITY);
        let (events_tx, events) = mpsc::channel(RECOVERY_EVENT_CAPACITY);
        let phase_cancellation = Arc::new(Mutex::new(CancellationToken::new()));
        let task = tokio::spawn(run_recovery(RecoveryTaskContext {
            commands: command_rx,
            events: events_tx,
            factory,
            replacement,
            prepared,
            initial_session_generation,
            initial_events,
        }));
        Ok(Self {
            handle: DeviceSessionRecoveryHandle {
                commands,
                request_generation: Arc::new(AtomicU64::new(0)),
                phase_cancellation,
            },
            events,
            task: Some(task),
        })
    }

    pub fn handle(&self) -> DeviceSessionRecoveryHandle {
        self.handle.clone()
    }

    pub async fn next_event(&mut self) -> Option<DeviceSessionRecoveryEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) -> Result<(), DeviceSessionRecoveryError> {
        self.handle.cancel_phase();
        let (response, reply) = oneshot::channel();
        self.handle
            .commands
            .send(RecoveryCommand::Shutdown(response))
            .await
            .map_err(|_| DeviceSessionRecoveryError::Stopped)?;
        reply
            .await
            .map_err(|_| DeviceSessionRecoveryError::Stopped)?;
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| DeviceSessionRecoveryError::Join(error.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for DeviceSessionRecoveryActor {
    fn drop(&mut self) {
        self.handle.cancel_phase();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug)]
enum RecoveryCommand {
    Recover {
        request_generation: u64,
        device: DeviceInfo,
        cancellation: CancellationToken,
    },
    Shutdown(oneshot::Sender<()>),
}

struct RecoveryTaskContext {
    commands: mpsc::Receiver<RecoveryCommand>,
    events: mpsc::Sender<DeviceSessionRecoveryEvent>,
    factory: Box<dyn DeviceSessionFactoryPort>,
    replacement: DeviceSessionReplacementHandle,
    prepared: PreparedCastDevice,
    initial_session_generation: NonZeroU64,
    initial_events: Box<dyn DeviceSessionEventPort>,
}

async fn run_recovery(context: RecoveryTaskContext) {
    let RecoveryTaskContext {
        mut commands,
        events,
        mut factory,
        mut replacement,
        prepared,
        initial_session_generation,
        initial_events,
    } = context;
    let mut last_session_generation = initial_session_generation;
    let mut current_events = Some(initial_events);
    loop {
        let next = match current_events.as_mut() {
            Some(session_events) => tokio::select! {
                command = commands.recv() => NextRecoveryInput::Command(command),
                event = session_events.next_event() => NextRecoveryInput::SessionEvent(event),
            },
            None => NextRecoveryInput::Command(commands.recv().await),
        };
        let command = match next {
            NextRecoveryInput::Command(Some(command)) => command,
            NextRecoveryInput::Command(None) => break,
            NextRecoveryInput::SessionEvent(event) => {
                let source = current_events
                    .take()
                    .expect("session event branch requires an event source");
                source.shutdown().await;
                let (session_generation, error) = match event {
                    Some(event) => (event.session_generation, event.error),
                    None => (
                        last_session_generation,
                        "Device-session event source stopped".into(),
                    ),
                };
                if events
                    .send(DeviceSessionRecoveryEvent::TransportFailed {
                        session_generation,
                        error: bounded_text(error, MAX_RECOVERY_ERROR_BYTES),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        match command {
            RecoveryCommand::Recover {
                request_generation,
                device,
                cancellation,
            } => {
                let Some(session_generation) = last_session_generation
                    .get()
                    .checked_add(1)
                    .and_then(NonZeroU64::new)
                else {
                    send_failure(
                        &events,
                        request_generation,
                        device,
                        "Device-session generation exhausted".into(),
                    )
                    .await;
                    continue;
                };
                // Every creation attempt consumes a generation, including an
                // attempt whose protocol outcome is ambiguous.
                last_session_generation = session_generation;
                if let Some(events) = current_events.take() {
                    events.shutdown().await;
                }
                let installation = match replacement.retire_current().await {
                    Ok(installation) => installation,
                    Err(error) => {
                        send_failure(
                            &events,
                            request_generation,
                            device,
                            format!("retire current Device session: {error}"),
                        )
                        .await;
                        continue;
                    }
                };
                let retired_session_cleanup_error = installation.retirement().cleanup_error.clone();
                let result = factory
                    .create_prepared_session(device.clone(), session_generation, cancellation)
                    .await;
                let recovered = match result {
                    Ok(recovered) => recovered,
                    Err(DeviceSessionFactoryError::Cancelled) => continue,
                    Err(error) => {
                        send_failure(&events, request_generation, device, error.to_string()).await;
                        continue;
                    }
                };

                let PreparedDeviceSession {
                    prepared: recovered_prepared,
                    session: recovered_session,
                    events: recovered_events,
                } = recovered;
                if let Err(error) = prepared.validate_recovery(&recovered_prepared) {
                    recovered_events.shutdown().await;
                    let cleanup = recovered_session
                        .stop(DeviceSessionStopReason::DaemonShutdown)
                        .await
                        .err()
                        .map(|error| error.to_string());
                    let diagnostic = match cleanup {
                        Some(cleanup) => format!(
                            "recovered Device session is incompatible: {error}; replacement cleanup also failed: {cleanup}"
                        ),
                        None => format!("recovered Device session is incompatible: {error}"),
                    };
                    send_failure(&events, request_generation, device, diagnostic).await;
                    continue;
                }

                match installation
                    .install(DeviceSessionReplacement {
                        session_generation,
                        session: recovered_session,
                    })
                    .await
                {
                    Ok(report) => {
                        current_events = Some(recovered_events);
                        let event = DeviceSessionRecoveryEvent::Ready {
                            request_generation,
                            device,
                            session_generation: report.installed_session_generation,
                            retired_session_cleanup_error,
                        };
                        if events.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        recovered_events.shutdown().await;
                        send_failure(
                            &events,
                            request_generation,
                            device,
                            format!("install recovered Device session: {error}"),
                        )
                        .await;
                    }
                }
            }
            RecoveryCommand::Shutdown(response) => {
                if let Some(events) = current_events.take() {
                    events.shutdown().await;
                }
                let _ = response.send(());
                return;
            }
        }
    }
}

enum NextRecoveryInput {
    Command(Option<RecoveryCommand>),
    SessionEvent(Option<crate::device_session_port::DeviceSessionEvent>),
}

async fn send_failure(
    events: &mpsc::Sender<DeviceSessionRecoveryEvent>,
    request_generation: u64,
    device: DeviceInfo,
    error: String,
) {
    let _ = events
        .send(DeviceSessionRecoveryEvent::Failed {
            request_generation,
            device,
            error: bounded_text(error, MAX_RECOVERY_ERROR_BYTES),
        })
        .await;
}

fn bounded_text(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut boundary = maximum.saturating_sub(3).min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value.push_str("...");
    value
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeviceSessionRecoveryError {
    #[error("DeviceSessionRecoveryActor requires a running Tokio runtime")]
    NoRuntime,
    #[error("Device-session recovery actor stopped")]
    Stopped,
    #[error("Device-session recovery generation exhausted")]
    GenerationExhausted,
    #[error("Device-session recovery actor task failed: {0}")]
    Join(String),
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use pronk_backend_protocol::{
        DeviceCapabilities, DisplayIdentity, DisplayMode, IdentitySource, VideoProfile,
    };
    use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID};
    use pronk_dbus::DeviceAvailability;

    use super::*;
    use crate::device_session_port::{
        DeviceMediaSetup, DeviceMediaStopReason, DeviceMediaSuspendReason, DeviceSessionError,
        DeviceSessionEvent,
    };
    use crate::replaceable_device_session::replaceable_device_session;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Stop(&'static str, DeviceSessionStopReason),
    }

    #[derive(Debug)]
    struct FakeSession {
        name: &'static str,
        calls: Arc<StdMutex<Vec<Call>>>,
    }

    #[async_trait]
    impl DeviceSessionPort for FakeSession {
        async fn configure_media(
            &mut self,
            _setup: DeviceMediaSetup,
        ) -> Result<(), DeviceSessionError> {
            Ok(())
        }

        async fn start_media(
            &mut self,
            _media_generation: NonZeroU64,
        ) -> Result<(), DeviceSessionError> {
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
            _media_generation: NonZeroU64,
            _reason: DeviceMediaStopReason,
        ) -> Result<(), DeviceSessionError> {
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

    #[derive(Debug)]
    struct TestEvents {
        events: VecDeque<DeviceSessionEvent>,
    }

    #[async_trait]
    impl DeviceSessionEventPort for TestEvents {
        async fn next_event(&mut self) -> Option<DeviceSessionEvent> {
            match self.events.pop_front() {
                Some(event) => Some(event),
                None => std::future::pending().await,
            }
        }

        async fn shutdown(self: Box<Self>) {}
    }

    #[derive(Debug)]
    struct FakeFactory {
        results: VecDeque<Result<PreparedDeviceSession, DeviceSessionFactoryError>>,
        generations: Arc<StdMutex<Vec<u64>>>,
    }

    #[derive(Debug)]
    struct CancellationObservingFactory {
        attempts: mpsc::UnboundedSender<u64>,
    }

    #[async_trait]
    impl DeviceSessionFactoryPort for CancellationObservingFactory {
        async fn create_prepared_session(
            &mut self,
            _device: DeviceInfo,
            session_generation: NonZeroU64,
            cancellation: CancellationToken,
        ) -> Result<PreparedDeviceSession, DeviceSessionFactoryError> {
            let _ = self.attempts.send(session_generation.get());
            cancellation.cancelled().await;
            Err(DeviceSessionFactoryError::Cancelled)
        }
    }

    #[async_trait]
    impl DeviceSessionFactoryPort for FakeFactory {
        async fn create_prepared_session(
            &mut self,
            _device: DeviceInfo,
            session_generation: NonZeroU64,
            _cancellation: CancellationToken,
        ) -> Result<PreparedDeviceSession, DeviceSessionFactoryError> {
            self.generations
                .lock()
                .unwrap()
                .push(session_generation.get());
            self.results
                .pop_front()
                .expect("test factory has a recovery result")
        }
    }

    fn device(connection: u64, discovery: u64, revision: u64) -> DeviceInfo {
        DeviceInfo {
            backend_id: "chromiacast".into(),
            device_id: "stable-tv-id".into(),
            display_name: "Living Room TV".into(),
            availability: DeviceAvailability::Available,
            connection_generation: connection,
            discovery_generation: discovery,
            device_revision: revision,
            metadata: Vec::new(),
        }
    }

    fn capabilities(product_name: &str) -> DeviceCapabilities {
        DeviceCapabilities {
            preparation_generation: 1,
            display_identity: DisplayIdentity {
                manufacturer_name: Some("Sony".into()),
                manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
                product_name: Some(product_name.into()),
                product_source: IdentitySource::SetupEndpoint,
                pnp_id: Some("SON".into()),
            },
            modes: vec![
                DisplayMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
                DisplayMode {
                    width: 640,
                    height: 480,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            ],
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 1920,
                max_height: 1080,
                max_refresh_millihz: 60_000,
            }],
            audio_profiles: Vec::new(),
            features: 0,
        }
    }

    fn prepared(device: DeviceInfo, product_name: &str) -> PreparedCastDevice {
        let resolver =
            PnpIdResolver::from_database("SON\tSony\n", &[], DEFAULT_SYNTHESIZER_PNP_ID).unwrap();
        PreparedCastDevice::from_capabilities(device, capabilities(product_name), &resolver, false)
            .unwrap()
    }

    fn session(name: &'static str, calls: &Arc<StdMutex<Vec<Call>>>) -> Box<dyn DeviceSessionPort> {
        Box::new(FakeSession {
            name,
            calls: Arc::clone(calls),
        })
    }

    fn pending_events() -> Box<dyn DeviceSessionEventPort> {
        Box::new(TestEvents {
            events: VecDeque::new(),
        })
    }

    #[tokio::test]
    async fn installs_a_compatible_session_then_forwards_its_terminal_event() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let generations = Arc::new(StdMutex::new(Vec::new()));
        let initial_device = device(1, 1, 1);
        let recovered_device = device(2, 3, 4);
        let baseline = prepared(initial_device, "Bravia XR");
        let recovered = prepared(recovered_device.clone(), "Bravia XR");
        let (media_port, _control, replacement) =
            replaceable_device_session(NonZeroU64::new(1).unwrap(), session("old", &calls));
        let terminal = DeviceSessionEvent {
            session_generation: NonZeroU64::new(2).unwrap(),
            error: "receiver timed out".into(),
        };
        let factory = FakeFactory {
            results: VecDeque::from([Ok(PreparedDeviceSession {
                prepared: recovered,
                session: session("new", &calls),
                events: Box::new(TestEvents {
                    events: VecDeque::from([terminal.clone()]),
                }),
            })]),
            generations: Arc::clone(&generations),
        };
        let mut actor = DeviceSessionRecoveryActor::spawn(
            Box::new(factory),
            replacement,
            baseline,
            NonZeroU64::new(1).unwrap(),
            pending_events(),
        )
        .unwrap();

        let request_generation = actor
            .handle()
            .recover(recovered_device.clone())
            .await
            .unwrap();
        assert_eq!(
            actor.next_event().await,
            Some(DeviceSessionRecoveryEvent::Ready {
                request_generation,
                device: recovered_device,
                session_generation: NonZeroU64::new(2).unwrap(),
                retired_session_cleanup_error: None,
            })
        );
        assert_eq!(
            actor.next_event().await,
            Some(DeviceSessionRecoveryEvent::TransportFailed {
                session_generation: terminal.session_generation,
                error: terminal.error,
            })
        );
        actor.shutdown().await.unwrap();
        media_port
            .stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .unwrap();

        assert_eq!(*generations.lock().unwrap(), vec![2]);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Stop("old", DeviceSessionStopReason::DaemonShutdown),
                Call::Stop("new", DeviceSessionStopReason::DaemonShutdown),
            ]
        );
    }

    #[tokio::test]
    async fn incompatible_recovery_is_cleaned_without_replacing_the_session() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let initial_device = device(1, 1, 1);
        let recovered_device = device(2, 2, 2);
        let baseline = prepared(initial_device, "Bravia XR");
        let incompatible = prepared(recovered_device.clone(), "Other TV");
        let (media_port, _control, replacement) =
            replaceable_device_session(NonZeroU64::new(1).unwrap(), session("old", &calls));
        let factory = FakeFactory {
            results: VecDeque::from([Ok(PreparedDeviceSession {
                prepared: incompatible,
                session: session("rejected", &calls),
                events: pending_events(),
            })]),
            generations: Arc::new(StdMutex::new(Vec::new())),
        };
        let mut actor = DeviceSessionRecoveryActor::spawn(
            Box::new(factory),
            replacement,
            baseline,
            NonZeroU64::new(1).unwrap(),
            pending_events(),
        )
        .unwrap();

        let request_generation = actor
            .handle()
            .recover(recovered_device.clone())
            .await
            .unwrap();
        let Some(DeviceSessionRecoveryEvent::Failed {
            request_generation: failed_request,
            device: failed_device,
            error,
        }) = actor.next_event().await
        else {
            panic!("incompatible recovery did not fail");
        };
        assert_eq!(failed_request, request_generation);
        assert_eq!(failed_device, recovered_device);
        assert!(error.contains("attached EDID"));
        actor.shutdown().await.unwrap();
        media_port
            .stop(DeviceSessionStopReason::DisplayRemoved)
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Stop("old", DeviceSessionStopReason::DaemonShutdown),
                Call::Stop("rejected", DeviceSessionStopReason::DaemonShutdown),
            ]
        );
    }

    #[tokio::test]
    async fn a_queued_recovery_cancels_the_command_bound_attempt_ahead_of_it() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let initial_device = device(1, 1, 1);
        let baseline = prepared(initial_device, "Bravia XR");
        let (media_port, _control, replacement) =
            replaceable_device_session(NonZeroU64::new(1).unwrap(), session("old", &calls));
        let (attempts, mut attempt_events) = mpsc::unbounded_channel();
        let actor = DeviceSessionRecoveryActor::spawn(
            Box::new(CancellationObservingFactory { attempts }),
            replacement,
            baseline,
            NonZeroU64::new(1).unwrap(),
            pending_events(),
        )
        .unwrap();
        let handle = actor.handle();

        handle.recover(device(2, 2, 2)).await.unwrap();
        handle.recover(device(3, 3, 3)).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), attempt_events.recv())
                .await
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), attempt_events.recv())
                .await
                .unwrap(),
            Some(3)
        );

        actor.shutdown().await.unwrap();
        media_port
            .stop(DeviceSessionStopReason::DaemonShutdown)
            .await
            .unwrap();
    }
}
