use std::time::Duration;

use futures_util::StreamExt;
use pronk_backend_protocol::{validate_error_text, Backend1Proxy};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zbus::{Connection, MessageStream};

use crate::connection::{BackendConnection, BACKEND_METHOD_TIMEOUT};
use crate::inventory::{
    ApplyOutcome, DeviceEvent, DeviceInventory, DeviceInventorySnapshot, InventoryError,
};

const DISCOVERY_EVENT_QUEUE: usize = 64;
const DISCOVERY_COMMAND_QUEUE: usize = 8;
const DISCOVERY_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryNotification {
    Changed(DeviceInventorySnapshot),
    Resynchronized {
        reason: String,
        snapshot: DeviceInventorySnapshot,
    },
    FatalError {
        connection_generation: u64,
        error_text: String,
    },
    ConnectionClosed,
    Failed(String),
}

#[derive(Debug)]
pub struct DiscoveryHandle {
    initial: DeviceInventorySnapshot,
    notifications: mpsc::Receiver<DiscoveryNotification>,
    commands: Option<mpsc::Sender<DiscoveryCommand>>,
    task: Option<JoinHandle<()>>,
}

impl DiscoveryHandle {
    pub fn initial(&self) -> &DeviceInventorySnapshot {
        &self.initial
    }

    pub async fn next_notification(&mut self) -> Option<DiscoveryNotification> {
        self.notifications.recv().await
    }

    pub async fn stop(mut self) -> Result<(), DiscoveryError> {
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .commands
            .take()
            .ok_or(DiscoveryError::ActorStopped)?
            .send(DiscoveryCommand::Stop(Some(response_tx)))
            .await
            .is_err()
        {
            self.join_task().await?;
            return Err(DiscoveryError::ActorStopped);
        }
        let response = match timeout(DISCOVERY_STOP_TIMEOUT, response_rx).await {
            Ok(Ok(response)) => response.map_err(DiscoveryError::StopFailed),
            Ok(Err(_)) => Err(DiscoveryError::ActorStopped),
            Err(_) => {
                self.abort_task().await;
                return Err(DiscoveryError::StopTimeout);
            }
        };
        let task = self.join_task().await;
        response?;
        task
    }

    pub(crate) async fn finish_after_terminal(mut self) -> Result<(), DiscoveryError> {
        self.commands.take();
        self.join_task().await
    }

    async fn join_task(&mut self) -> Result<(), DiscoveryError> {
        if let Some(task) = self.task.take() {
            task.await.map_err(DiscoveryError::Task)?;
        }
        Ok(())
    }

    async fn abort_task(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.try_send(DiscoveryCommand::Stop(None));
        }
        if let Some(task) = self.task.take() {
            // `stop` is the acknowledged teardown path. Dropping the owner
            // must not detach a wedged protocol monitor.
            task.abort();
        }
    }
}

impl BackendConnection {
    pub async fn start_discovery(&self) -> Result<DiscoveryHandle, DiscoveryError> {
        let (initial_tx, initial_rx) = oneshot::channel();
        let (notification_tx, notification_rx) = mpsc::channel(DISCOVERY_EVENT_QUEUE);
        let (command_tx, command_rx) = mpsc::channel(DISCOVERY_COMMAND_QUEUE);
        let task = tokio::spawn(run_discovery(
            self.connection().clone(),
            self.endpoint().backend_id().into(),
            self.connection_generation(),
            initial_tx,
            notification_tx,
            command_rx,
        ));
        let mut task = AbortOnDropTask::new(task);
        let initial = match timeout(BACKEND_METHOD_TIMEOUT, initial_rx).await {
            Ok(Ok(Ok(initial))) => initial,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => return Err(DiscoveryError::ActorStopped),
            Err(_) => return Err(DiscoveryError::StartTimeout),
        };
        Ok(DiscoveryHandle {
            initial,
            notifications: notification_rx,
            commands: Some(command_tx),
            task: Some(task.take()),
        })
    }
}

struct AbortOnDropTask(Option<JoinHandle<()>>);

impl AbortOnDropTask {
    fn new(task: JoinHandle<()>) -> Self {
        Self(Some(task))
    }

    fn take(&mut self) -> JoinHandle<()> {
        self.0.take().expect("discovery task already taken")
    }
}

impl Drop for AbortOnDropTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[derive(Debug)]
enum DiscoveryCommand {
    Stop(Option<oneshot::Sender<Result<(), String>>>),
}

async fn run_discovery(
    connection: Connection,
    expected_backend_id: String,
    connection_generation: u64,
    initial_tx: oneshot::Sender<Result<DeviceInventorySnapshot, DiscoveryError>>,
    notifications: mpsc::Sender<DiscoveryNotification>,
    mut commands: mpsc::Receiver<DiscoveryCommand>,
) {
    let proxy = match Backend1Proxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(error) => {
            let _ = initial_tx.send(Err(DiscoveryError::Protocol(error)));
            return;
        }
    };
    let mut added = match proxy.receive_device_added().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = initial_tx.send(Err(DiscoveryError::Protocol(error)));
            return;
        }
    };
    let mut changed = match proxy.receive_device_changed().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = initial_tx.send(Err(DiscoveryError::Protocol(error)));
            return;
        }
    };
    let mut removed = match proxy.receive_device_removed().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = initial_tx.send(Err(DiscoveryError::Protocol(error)));
            return;
        }
    };
    let mut fatal = match proxy.receive_fatal_error().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = initial_tx.send(Err(DiscoveryError::Protocol(error)));
            return;
        }
    };

    let discovery_generation = match timeout(BACKEND_METHOD_TIMEOUT, proxy.start_discovery()).await
    {
        Ok(Ok(generation)) => generation,
        Ok(Err(error)) => {
            let _ = initial_tx.send(Err(DiscoveryError::Protocol(error)));
            return;
        }
        Err(_) => {
            let _ = initial_tx.send(Err(DiscoveryError::StartTimeout));
            return;
        }
    };
    let snapshot = match fetch_snapshot(&proxy, &expected_backend_id, discovery_generation).await {
        Ok(inventory) => inventory,
        Err(error) => {
            let _ = initial_tx.send(Err(error));
            return;
        }
    };
    let initial = snapshot.snapshot();
    if initial_tx.send(Ok(initial)).is_err() {
        let _ = stop_discovery(&proxy, discovery_generation).await;
        return;
    }
    let mut inventory = snapshot;
    let mut messages = MessageStream::from(&connection);

    loop {
        tokio::select! {
            command = commands.recv() => {
                let response = stop_discovery(&proxy, discovery_generation)
                    .await
                    .map_err(|error| error.to_string());
                if let Some(DiscoveryCommand::Stop(Some(sender))) = command {
                    let _ = sender.send(response);
                }
                break;
            }
            signal = added.next() => {
                let Some(signal) = signal else {
                    send_closed(&notifications).await;
                    break;
                };
                let event = match signal.args() {
                    Ok(args) => DeviceEvent::Added {
                        discovery_generation: *args.discovery_generation(),
                        revision: *args.revision(),
                        device: args.device().clone(),
                    },
                    Err(error) => {
                        send_failed(&notifications, error.to_string()).await;
                        break;
                    }
                };
                if !process_event(&proxy, &expected_backend_id, &mut inventory, event, &notifications).await {
                    break;
                }
            }
            signal = changed.next() => {
                let Some(signal) = signal else {
                    send_closed(&notifications).await;
                    break;
                };
                let event = match signal.args() {
                    Ok(args) => DeviceEvent::Changed {
                        discovery_generation: *args.discovery_generation(),
                        revision: *args.revision(),
                        device: args.device().clone(),
                    },
                    Err(error) => {
                        send_failed(&notifications, error.to_string()).await;
                        break;
                    }
                };
                if !process_event(&proxy, &expected_backend_id, &mut inventory, event, &notifications).await {
                    break;
                }
            }
            signal = removed.next() => {
                let Some(signal) = signal else {
                    send_closed(&notifications).await;
                    break;
                };
                let event = match signal.args() {
                    Ok(args) => DeviceEvent::Removed {
                        discovery_generation: *args.discovery_generation(),
                        revision: *args.revision(),
                        device: args.device().clone(),
                    },
                    Err(error) => {
                        send_failed(&notifications, error.to_string()).await;
                        break;
                    }
                };
                if !process_event(&proxy, &expected_backend_id, &mut inventory, event, &notifications).await {
                    break;
                }
            }
            signal = fatal.next() => {
                let Some(signal) = signal else {
                    send_closed(&notifications).await;
                    break;
                };
                let args = match signal.args() {
                    Ok(args) => args,
                    Err(error) => {
                        send_failed(&notifications, error.to_string()).await;
                        break;
                    }
                };
                let actual_generation = *args.connection_generation();
                let error_text = args.error_text().clone();
                if actual_generation != connection_generation {
                    send_failed(
                        &notifications,
                        format!(
                            "FatalError connection generation {actual_generation} differs from {connection_generation}"
                        ),
                    )
                    .await;
                    break;
                }
                if let Err(error) = validate_error_text(&error_text) {
                    send_failed(&notifications, error.to_string()).await;
                    break;
                }
                let _ = notifications
                    .send(DiscoveryNotification::FatalError {
                        connection_generation,
                        error_text,
                    })
                    .await;
                break;
            }
            message = messages.next() => {
                match message {
                    None => {
                        send_closed(&notifications).await;
                        break;
                    }
                    Some(Err(zbus::Error::InputOutput(_))) => {
                        send_closed(&notifications).await;
                        break;
                    }
                    Some(Err(error)) => {
                        send_failed(&notifications, error.to_string()).await;
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

async fn process_event(
    proxy: &Backend1Proxy<'_>,
    expected_backend_id: &str,
    inventory: &mut DeviceInventory,
    event: DeviceEvent,
    notifications: &mpsc::Sender<DiscoveryNotification>,
) -> bool {
    match inventory.apply(event) {
        Ok(ApplyOutcome::Changed) => notifications
            .send(DiscoveryNotification::Changed(inventory.snapshot()))
            .await
            .is_ok(),
        Ok(ApplyOutcome::IgnoredCoveredBySnapshot | ApplyOutcome::IgnoredDuplicate) => true,
        Err(error) => {
            let reason = error.to_string();
            let discovery_generation = inventory.snapshot().discovery_generation;
            match fetch_snapshot(proxy, expected_backend_id, discovery_generation).await {
                Ok(replacement) => {
                    *inventory = replacement;
                    notifications
                        .send(DiscoveryNotification::Resynchronized {
                            reason,
                            snapshot: inventory.snapshot(),
                        })
                        .await
                        .is_ok()
                }
                Err(error) => {
                    send_failed(notifications, error.to_string()).await;
                    false
                }
            }
        }
    }
}

async fn fetch_snapshot(
    proxy: &Backend1Proxy<'_>,
    expected_backend_id: &str,
    discovery_generation: u64,
) -> Result<DeviceInventory, DiscoveryError> {
    let snapshot = timeout(BACKEND_METHOD_TIMEOUT, proxy.list_devices())
        .await
        .map_err(|_| DiscoveryError::SnapshotTimeout)?
        .map_err(DiscoveryError::Protocol)?;
    if snapshot.discovery_generation != discovery_generation {
        return Err(DiscoveryError::SnapshotGeneration {
            expected: discovery_generation,
            actual: snapshot.discovery_generation,
        });
    }
    DeviceInventory::from_snapshot(expected_backend_id, snapshot).map_err(DiscoveryError::Inventory)
}

async fn stop_discovery(
    proxy: &Backend1Proxy<'_>,
    discovery_generation: u64,
) -> Result<(), DiscoveryError> {
    timeout(
        DISCOVERY_STOP_TIMEOUT,
        proxy.stop_discovery(discovery_generation),
    )
    .await
    .map_err(|_| DiscoveryError::StopTimeout)?
    .map_err(DiscoveryError::Protocol)
}

async fn send_closed(notifications: &mpsc::Sender<DiscoveryNotification>) {
    let _ = notifications
        .send(DiscoveryNotification::ConnectionClosed)
        .await;
}

async fn send_failed(notifications: &mpsc::Sender<DiscoveryNotification>, error: String) {
    let _ = notifications
        .send(DiscoveryNotification::Failed(error))
        .await;
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("backend discovery protocol failed: {0}")]
    Protocol(zbus::Error),
    #[error("Backend1.StartDiscovery timed out")]
    StartTimeout,
    #[error("Backend1.ListDevices timed out")]
    SnapshotTimeout,
    #[error(
        "device snapshot generation {actual} differs from StartDiscovery generation {expected}"
    )]
    SnapshotGeneration { expected: u64, actual: u64 },
    #[error("invalid device inventory: {0}")]
    Inventory(InventoryError),
    #[error("backend discovery actor stopped unexpectedly")]
    ActorStopped,
    #[error("Backend1.StopDiscovery timed out")]
    StopTimeout,
    #[error("Backend1.StopDiscovery failed: {0}")]
    StopFailed(String),
    #[error("backend discovery task failed: {0}")]
    Task(tokio::task::JoinError),
}
