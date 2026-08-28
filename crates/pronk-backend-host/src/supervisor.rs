use std::sync::Arc;
use std::time::Duration;

use pronk_backend_protocol::{BackendInfo, DeviceAvailability};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, timeout_at, Instant};

use crate::connection::{BackendConnection, BackendRegistrationValidator, BACKEND_METHOD_TIMEOUT};
use crate::discovery::{DiscoveryHandle, DiscoveryNotification};
use crate::endpoint::BackendEndpoint;
use crate::inventory::DeviceInventorySnapshot;
use crate::session::{
    create_connected_session, BackendSessionError, BackendSessionHandle, BackendSessionRequest,
};

const SUPERVISOR_COMMAND_QUEUE: usize = 8;
const SUPERVISOR_EVENT_QUEUE: usize = 64;
const SUPERVISOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendReconnectPolicy {
    pub max_reconnect_attempts: u32,
    pub initial_delay: Duration,
    pub maximum_delay: Duration,
    pub stable_after: Duration,
}

impl BackendReconnectPolicy {
    pub fn new(
        max_reconnect_attempts: u32,
        initial_delay: Duration,
        maximum_delay: Duration,
        stable_after: Duration,
    ) -> Result<Self, BackendSupervisorError> {
        let policy = Self {
            max_reconnect_attempts,
            initial_delay,
            maximum_delay,
            stable_after,
        };
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), BackendSupervisorError> {
        if self.initial_delay > self.maximum_delay {
            return Err(BackendSupervisorError::InvalidReconnectPolicy(
                "initial reconnect delay exceeds maximum delay".into(),
            ));
        }
        if self.stable_after.is_zero() {
            return Err(BackendSupervisorError::InvalidReconnectPolicy(
                "backend stability interval must be nonzero".into(),
            ));
        }
        Ok(())
    }

    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        debug_assert!(attempt > 0);
        let multiplier = 1_u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.initial_delay
            .saturating_mul(multiplier)
            .min(self.maximum_delay)
    }
}

impl Default for BackendReconnectPolicy {
    fn default() -> Self {
        Self {
            max_reconnect_attempts: 5,
            initial_delay: Duration::from_millis(250),
            maximum_delay: Duration::from_secs(4),
            stable_after: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendDisconnectReason {
    ConnectionClosed,
    FatalError(String),
    DiscoveryFailed(String),
    DiscoveryActorStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSupervisorEvent {
    Connecting {
        connection_generation: u64,
    },
    Connected {
        connection_generation: u64,
        negotiated_minor: u16,
        info: BackendInfo,
        inventory: DeviceInventorySnapshot,
    },
    InventoryChanged {
        connection_generation: u64,
        inventory: DeviceInventorySnapshot,
    },
    InventoryResynchronized {
        connection_generation: u64,
        reason: String,
        inventory: DeviceInventorySnapshot,
    },
    ConnectionFailed {
        connection_generation: u64,
        error: String,
    },
    Disconnected {
        connection_generation: u64,
        reason: BackendDisconnectReason,
        unavailable_inventory: DeviceInventorySnapshot,
    },
    ReconnectScheduled {
        next_connection_generation: u64,
        attempt: u32,
        delay: Duration,
    },
    ReconnectExhausted {
        last_connection_generation: u64,
        attempts: u32,
    },
    Stopped {
        last_connection_generation: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendShutdownReport {
    pub last_connection_generation: Option<u64>,
    pub graceful: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BackendHandle {
    backend_id: String,
    commands: mpsc::Sender<SupervisorCommand>,
}

impl BackendHandle {
    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }

    pub async fn retry_now(&self) -> Result<(), BackendRetryError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(SupervisorCommand::Retry(response_tx))
            .await
            .map_err(|_| BackendRetryError::SupervisorStopped)?;
        response_rx
            .await
            .map_err(|_| BackendRetryError::SupervisorStopped)?
    }

    pub async fn create_session(
        &self,
        request: BackendSessionRequest,
    ) -> Result<BackendSessionHandle, BackendSessionError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(SupervisorCommand::CreateSession {
                request,
                response: response_tx,
            })
            .await
            .map_err(|_| BackendSessionError::SupervisorStopped)?;
        response_rx
            .await
            .map_err(|_| BackendSessionError::SupervisorStopped)?
    }
}

#[derive(Debug)]
pub struct BackendSupervisor {
    handle: BackendHandle,
    events: mpsc::Receiver<BackendSupervisorEvent>,
    task: Option<JoinHandle<Result<(), SupervisorTaskError>>>,
}

impl BackendSupervisor {
    pub fn spawn(
        endpoint: BackendEndpoint,
        initial_connection_generation: u64,
        registration_validator: Arc<dyn BackendRegistrationValidator>,
        reconnect_policy: BackendReconnectPolicy,
    ) -> Result<Self, BackendSupervisorError> {
        if initial_connection_generation == 0 {
            return Err(BackendSupervisorError::ZeroConnectionGeneration);
        }
        reconnect_policy.validate()?;
        let (command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_QUEUE);
        let (event_tx, event_rx) = mpsc::channel(SUPERVISOR_EVENT_QUEUE);
        let handle = BackendHandle {
            backend_id: endpoint.backend_id().into(),
            commands: command_tx,
        };
        let task = tokio::spawn(run_supervisor(
            endpoint,
            initial_connection_generation,
            registration_validator,
            reconnect_policy,
            command_rx,
            event_tx,
        ));
        Ok(Self {
            handle,
            events: event_rx,
            task: Some(task),
        })
    }

    pub fn handle(&self) -> BackendHandle {
        self.handle.clone()
    }

    pub async fn next_event(&mut self) -> Option<BackendSupervisorEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) -> Result<BackendShutdownReport, BackendSupervisorError> {
        let deadline = Instant::now() + SUPERVISOR_SHUTDOWN_TIMEOUT;
        let (response_tx, response_rx) = oneshot::channel();
        timeout_at(
            deadline,
            self.handle
                .commands
                .send(SupervisorCommand::Shutdown(response_tx)),
        )
        .await
        .map_err(|_| BackendSupervisorError::ShutdownTimeout)?
        .map_err(|_| BackendSupervisorError::SupervisorStopped)?;
        let report = timeout_at(deadline, response_rx)
            .await
            .map_err(|_| BackendSupervisorError::ShutdownTimeout)?
            .map_err(|_| BackendSupervisorError::SupervisorStopped)?;
        self.join_task_until(deadline).await?;
        Ok(report)
    }

    async fn join_task_until(&mut self, deadline: Instant) -> Result<(), BackendSupervisorError> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        if task.is_finished() {
            return task
                .await
                .map_err(BackendSupervisorError::Task)?
                .map_err(|error| BackendSupervisorError::TaskFailed(error.to_string()));
        }
        match timeout_at(deadline, &mut task).await {
            Ok(result) => result
                .map_err(BackendSupervisorError::Task)?
                .map_err(|error| BackendSupervisorError::TaskFailed(error.to_string())),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(BackendSupervisorError::ShutdownTimeout)
            }
        }
    }
}

impl Drop for BackendSupervisor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Explicit `shutdown` performs protocol teardown. Unexpected owner
            // loss must not orphan a reconnect loop through cloned handles.
            task.abort();
        }
    }
}

#[derive(Debug)]
enum SupervisorCommand {
    Retry(oneshot::Sender<Result<(), BackendRetryError>>),
    CreateSession {
        request: BackendSessionRequest,
        response: oneshot::Sender<Result<BackendSessionHandle, BackendSessionError>>,
    },
    Shutdown(oneshot::Sender<BackendShutdownReport>),
}

async fn run_supervisor(
    endpoint: BackendEndpoint,
    initial_connection_generation: u64,
    registration_validator: Arc<dyn BackendRegistrationValidator>,
    reconnect_policy: BackendReconnectPolicy,
    mut commands: mpsc::Receiver<SupervisorCommand>,
    events: mpsc::Sender<BackendSupervisorEvent>,
) -> Result<(), SupervisorTaskError> {
    let mut connection_generation = initial_connection_generation;
    let mut last_connected_generation = None;
    let mut reconnect_attempts = 0;

    loop {
        emit(
            &events,
            BackendSupervisorEvent::Connecting {
                connection_generation,
            },
        )?;
        let connect = BackendConnection::connect(
            endpoint.clone(),
            connection_generation,
            registration_validator.clone(),
        );
        tokio::pin!(connect);
        let connection = loop {
            tokio::select! {
                result = &mut connect => break result,
                command = commands.recv() => match command {
                    Some(SupervisorCommand::Retry(response)) => {
                        let _ = response.send(Err(BackendRetryError::AlreadyConnecting));
                        continue;
                    }
                    Some(SupervisorCommand::CreateSession { response, .. }) => {
                        let _ = response.send(Err(BackendSessionError::BackendUnavailable));
                        continue;
                    }
                    Some(SupervisorCommand::Shutdown(response)) => {
                        let report = BackendShutdownReport {
                            last_connection_generation: last_connected_generation,
                            graceful: true,
                            errors: Vec::new(),
                        };
                        let _ = response.send(report);
                        let _ = events.try_send(BackendSupervisorEvent::Stopped {
                            last_connection_generation: last_connected_generation,
                        });
                        return Ok(());
                    }
                    None => return Ok(()),
                },
            }
        };

        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                emit(
                    &events,
                    BackendSupervisorEvent::ConnectionFailed {
                        connection_generation,
                        error: error.to_string(),
                    },
                )?;
                match wait_for_retry(
                    connection_generation,
                    &mut reconnect_attempts,
                    &reconnect_policy,
                    last_connected_generation,
                    &mut commands,
                    &events,
                )
                .await?
                {
                    RetryAction::Retry(next_generation) => {
                        connection_generation = next_generation;
                        continue;
                    }
                    RetryAction::Stop => return Ok(()),
                }
            }
        };
        last_connected_generation = Some(connection_generation);
        let connected_at = Instant::now();

        let mut discovery = match connection.start_discovery().await {
            Ok(discovery) => discovery,
            Err(error) => {
                let failure = format!("start discovery: {error}");
                let _ = connection.shutdown().await;
                let _ = connection.close().await;
                emit(
                    &events,
                    BackendSupervisorEvent::ConnectionFailed {
                        connection_generation,
                        error: failure,
                    },
                )?;
                match wait_for_retry(
                    connection_generation,
                    &mut reconnect_attempts,
                    &reconnect_policy,
                    last_connected_generation,
                    &mut commands,
                    &events,
                )
                .await?
                {
                    RetryAction::Retry(next_generation) => {
                        connection_generation = next_generation;
                        continue;
                    }
                    RetryAction::Stop => return Ok(()),
                }
            }
        };
        let mut inventory = discovery.initial().clone();
        emit(
            &events,
            BackendSupervisorEvent::Connected {
                connection_generation,
                negotiated_minor: connection.negotiated_minor(),
                info: connection.info().clone(),
                inventory: inventory.clone(),
            },
        )?;

        let terminal_reason = loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(SupervisorCommand::Retry(response)) => {
                        let _ = response.send(Err(BackendRetryError::AlreadyConnected));
                    }
                    Some(SupervisorCommand::CreateSession { request, response }) => {
                        let result = create_connected_session(
                            connection.connection(),
                            endpoint.backend_id(),
                            connection_generation,
                            &inventory,
                            request,
                        ).await;
                        let _ = response.send(result);
                    }
                    Some(SupervisorCommand::Shutdown(response)) => {
                        let report = stop_connected(
                            connection,
                            discovery,
                            Some(connection_generation),
                            registration_validator.as_ref(),
                        )
                        .await;
                        let _ = response.send(report);
                        let _ = events.try_send(BackendSupervisorEvent::Stopped {
                            last_connection_generation: Some(connection_generation),
                        });
                        return Ok(());
                    }
                    None => {
                        let _ = stop_connected(
                            connection,
                            discovery,
                            Some(connection_generation),
                            registration_validator.as_ref(),
                        )
                        .await;
                        return Ok(());
                    }
                },
                notification = discovery.next_notification() => match notification {
                    Some(DiscoveryNotification::Changed(snapshot)) => {
                        inventory = snapshot.clone();
                        emit(
                            &events,
                            BackendSupervisorEvent::InventoryChanged {
                                connection_generation,
                                inventory: snapshot,
                            },
                        )?;
                    }
                    Some(DiscoveryNotification::Resynchronized { reason, snapshot }) => {
                        inventory = snapshot.clone();
                        emit(
                            &events,
                            BackendSupervisorEvent::InventoryResynchronized {
                                connection_generation,
                                reason,
                                inventory: snapshot,
                            },
                        )?;
                    }
                    Some(DiscoveryNotification::FatalError { error_text, .. }) => {
                        break BackendDisconnectReason::FatalError(error_text);
                    }
                    Some(DiscoveryNotification::ConnectionClosed) => {
                        break BackendDisconnectReason::ConnectionClosed;
                    }
                    Some(DiscoveryNotification::Failed(error)) => {
                        break BackendDisconnectReason::DiscoveryFailed(error);
                    }
                    None => break BackendDisconnectReason::DiscoveryActorStopped,
                },
            }
        };

        let _ = discovery.finish_after_terminal().await;
        if terminal_reason != BackendDisconnectReason::ConnectionClosed {
            let _ = connection.shutdown().await;
            let _ = timeout(BACKEND_METHOD_TIMEOUT, connection.wait_for_eof()).await;
        }
        let _ = connection.close().await;
        let unavailable_inventory = mark_unavailable(inventory);
        emit(
            &events,
            BackendSupervisorEvent::Disconnected {
                connection_generation,
                reason: terminal_reason,
                unavailable_inventory,
            },
        )?;

        if connected_at.elapsed() >= reconnect_policy.stable_after {
            reconnect_attempts = 0;
        }
        match wait_for_retry(
            connection_generation,
            &mut reconnect_attempts,
            &reconnect_policy,
            last_connected_generation,
            &mut commands,
            &events,
        )
        .await?
        {
            RetryAction::Retry(next_generation) => connection_generation = next_generation,
            RetryAction::Stop => return Ok(()),
        }
    }
}

async fn wait_for_retry(
    failed_generation: u64,
    reconnect_attempts: &mut u32,
    policy: &BackendReconnectPolicy,
    last_connected_generation: Option<u64>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    events: &mpsc::Sender<BackendSupervisorEvent>,
) -> Result<RetryAction, SupervisorTaskError> {
    if *reconnect_attempts >= policy.max_reconnect_attempts {
        emit(
            events,
            BackendSupervisorEvent::ReconnectExhausted {
                last_connection_generation: failed_generation,
                attempts: *reconnect_attempts,
            },
        )?;
        loop {
            match commands.recv().await {
                Some(SupervisorCommand::Retry(response)) => {
                    let next_generation = next_generation(failed_generation)?;
                    *reconnect_attempts = 0;
                    let _ = response.send(Ok(()));
                    return Ok(RetryAction::Retry(next_generation));
                }
                Some(SupervisorCommand::CreateSession { response, .. }) => {
                    let _ = response.send(Err(BackendSessionError::BackendUnavailable));
                }
                Some(SupervisorCommand::Shutdown(response)) => {
                    let report = BackendShutdownReport {
                        last_connection_generation: last_connected_generation,
                        graceful: true,
                        errors: Vec::new(),
                    };
                    let _ = response.send(report);
                    let _ = events.try_send(BackendSupervisorEvent::Stopped {
                        last_connection_generation: last_connected_generation,
                    });
                    return Ok(RetryAction::Stop);
                }
                None => return Ok(RetryAction::Stop),
            }
        }
    }

    *reconnect_attempts += 1;
    let attempt = *reconnect_attempts;
    let delay = policy.delay_for_attempt(attempt);
    let next_generation = next_generation(failed_generation)?;
    emit(
        events,
        BackendSupervisorEvent::ReconnectScheduled {
            next_connection_generation: next_generation,
            attempt,
            delay,
        },
    )?;
    let retry_delay = sleep(delay);
    tokio::pin!(retry_delay);
    loop {
        tokio::select! {
            () = &mut retry_delay => return Ok(RetryAction::Retry(next_generation)),
            command = commands.recv() => match command {
                Some(SupervisorCommand::Retry(response)) => {
                    *reconnect_attempts = 0;
                    let _ = response.send(Ok(()));
                    return Ok(RetryAction::Retry(next_generation));
                }
                Some(SupervisorCommand::CreateSession { response, .. }) => {
                    let _ = response.send(Err(BackendSessionError::BackendUnavailable));
                }
                Some(SupervisorCommand::Shutdown(response)) => {
                    let report = BackendShutdownReport {
                        last_connection_generation: last_connected_generation,
                        graceful: true,
                        errors: Vec::new(),
                    };
                    let _ = response.send(report);
                    let _ = events.try_send(BackendSupervisorEvent::Stopped {
                        last_connection_generation: last_connected_generation,
                    });
                    return Ok(RetryAction::Stop);
                }
                None => return Ok(RetryAction::Stop),
            }
        }
    }
}

async fn stop_connected(
    connection: BackendConnection,
    discovery: DiscoveryHandle,
    connection_generation: Option<u64>,
    registration_validator: &dyn BackendRegistrationValidator,
) -> BackendShutdownReport {
    let mut errors = Vec::new();
    let endpoint = connection.endpoint().clone();
    let info = connection.info().clone();
    if let Err(error) = discovery.stop().await {
        errors.push(error.to_string());
    }
    let mut needs_forced_stop = false;
    let shutdown_sent = match connection.shutdown().await {
        Ok(()) => true,
        Err(error) => {
            errors.push(error.to_string());
            needs_forced_stop = true;
            false
        }
    };
    if shutdown_sent {
        match timeout(BACKEND_METHOD_TIMEOUT, connection.wait_for_eof()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                errors.push(error.to_string());
                needs_forced_stop = true;
            }
            Err(_) => {
                errors.push("backend did not close after Shutdown".into());
                needs_forced_stop = true;
            }
        }
    }
    if needs_forced_stop {
        if let Err(error) = registration_validator
            .stop_validated_instance(&endpoint, &info)
            .await
        {
            errors.push(error.to_string());
        }
    }
    if !errors.is_empty() {
        let _ = connection.close().await;
    }
    BackendShutdownReport {
        last_connection_generation: connection_generation,
        graceful: errors.is_empty(),
        errors,
    }
}

fn mark_unavailable(mut snapshot: DeviceInventorySnapshot) -> DeviceInventorySnapshot {
    for device in &mut snapshot.devices {
        device.availability = DeviceAvailability::Unavailable;
    }
    snapshot
}

fn next_generation(generation: u64) -> Result<u64, SupervisorTaskError> {
    generation
        .checked_add(1)
        .ok_or(SupervisorTaskError::ConnectionGenerationExhausted)
}

fn emit(
    events: &mpsc::Sender<BackendSupervisorEvent>,
    event: BackendSupervisorEvent,
) -> Result<(), SupervisorTaskError> {
    events.try_send(event).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => SupervisorTaskError::EventQueueFull,
        mpsc::error::TrySendError::Closed(_) => SupervisorTaskError::EventConsumerClosed,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAction {
    Retry(u64),
    Stop,
}

#[derive(Debug, Error)]
enum SupervisorTaskError {
    #[error("backend supervisor event queue is full")]
    EventQueueFull,
    #[error("backend supervisor event consumer is closed")]
    EventConsumerClosed,
    #[error("backend connection generation is exhausted")]
    ConnectionGenerationExhausted,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BackendRetryError {
    #[error("backend is already connected")]
    AlreadyConnected,
    #[error("backend connection attempt is already in progress")]
    AlreadyConnecting,
    #[error("backend supervisor has stopped")]
    SupervisorStopped,
}

#[derive(Debug, Error)]
pub enum BackendSupervisorError {
    #[error("initial backend connection generation must be nonzero")]
    ZeroConnectionGeneration,
    #[error("invalid backend reconnect policy: {0}")]
    InvalidReconnectPolicy(String),
    #[error("backend supervisor has stopped")]
    SupervisorStopped,
    #[error("backend supervisor shutdown timed out")]
    ShutdownTimeout,
    #[error("backend supervisor task failed: {0}")]
    Task(tokio::task::JoinError),
    #[error("backend supervisor failed: {0}")]
    TaskFailed(String),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pronk_backend_protocol::DeviceInfo;
    use tokio::time::timeout;

    use super::*;
    use crate::{BackendEndpoint, ExactRegistrationValidator};

    #[test]
    fn reconnect_policy_is_bounded_and_saturating() {
        let policy = BackendReconnectPolicy::new(
            40,
            Duration::from_millis(10),
            Duration::from_millis(80),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(10));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(40));
        assert_eq!(policy.delay_for_attempt(40), Duration::from_millis(80));
        assert!(BackendReconnectPolicy::new(
            1,
            Duration::from_secs(2),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .is_err());
    }

    #[test]
    fn disconnect_preserves_identity_and_marks_every_device_unavailable() {
        let snapshot = DeviceInventorySnapshot {
            discovery_generation: 3,
            revision: 7,
            devices: Vec::new(),
        };
        let mut snapshot = snapshot;
        snapshot.devices.push(DeviceInfo {
            backend_id: "mock".into(),
            device_id: "living-room".into(),
            display_name: "Living Room".into(),
            availability: DeviceAvailability::Available,
            metadata: Vec::new(),
        });
        let unavailable = mark_unavailable(snapshot);
        assert_eq!(unavailable.discovery_generation, 3);
        assert_eq!(unavailable.revision, 7);
        assert_eq!(unavailable.devices[0].device_id, "living-room");
        assert_eq!(
            unavailable.devices[0].availability,
            DeviceAvailability::Unavailable
        );
    }

    #[tokio::test]
    async fn exhausted_supervisor_stays_owned_and_accepts_manual_retry() {
        let endpoint = BackendEndpoint::new(
            "mock",
            format!(
                "/tmp/pronk-supervisor-no-listener-{}.sock",
                std::process::id()
            ),
            "pronk-backend-mock@.service",
        )
        .unwrap();
        let policy =
            BackendReconnectPolicy::new(0, Duration::ZERO, Duration::ZERO, Duration::from_secs(1))
                .unwrap();
        let mut supervisor = BackendSupervisor::spawn(
            endpoint,
            7,
            Arc::new(ExactRegistrationValidator::new("mock", "development")),
            policy,
        )
        .unwrap();

        assert_eq!(
            next_event(&mut supervisor).await,
            BackendSupervisorEvent::Connecting {
                connection_generation: 7
            }
        );
        assert!(matches!(
            next_event(&mut supervisor).await,
            BackendSupervisorEvent::ConnectionFailed {
                connection_generation: 7,
                ..
            }
        ));
        assert_eq!(
            next_event(&mut supervisor).await,
            BackendSupervisorEvent::ReconnectExhausted {
                last_connection_generation: 7,
                attempts: 0,
            }
        );

        supervisor.handle().retry_now().await.unwrap();
        assert_eq!(
            next_event(&mut supervisor).await,
            BackendSupervisorEvent::Connecting {
                connection_generation: 8
            }
        );
        assert!(matches!(
            next_event(&mut supervisor).await,
            BackendSupervisorEvent::ConnectionFailed {
                connection_generation: 8,
                ..
            }
        ));
        assert_eq!(
            next_event(&mut supervisor).await,
            BackendSupervisorEvent::ReconnectExhausted {
                last_connection_generation: 8,
                attempts: 0,
            }
        );
        let report = supervisor.shutdown().await.unwrap();
        assert!(report.graceful);
        assert_eq!(report.last_connection_generation, None);
    }

    async fn next_event(supervisor: &mut BackendSupervisor) -> BackendSupervisorEvent {
        timeout(Duration::from_secs(1), supervisor.next_event())
            .await
            .expect("supervisor event timed out")
            .expect("supervisor event stream ended")
    }
}
