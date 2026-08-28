//! Passive, generation-validating monitor for one backend session object.

use std::time::Duration;

use futures_util::StreamExt;
use pronk_backend_protocol::{validate_error_text, BackendSession1Proxy};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MessageStream};

use crate::{BackendSessionError, BackendSessionHandle};

const SESSION_MONITOR_START_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_EVENT_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSessionEvent {
    Disconnected {
        session_generation: u64,
        error_text: String,
    },
    FatalError {
        session_generation: u64,
        error_text: String,
    },
    ConnectionClosed {
        session_generation: u64,
    },
    MonitorFailed {
        session_generation: u64,
        error_text: String,
    },
}

#[derive(Debug)]
pub struct BackendSessionMonitor {
    events: mpsc::Receiver<BackendSessionEvent>,
    task: Option<JoinHandle<()>>,
}

impl BackendSessionMonitor {
    pub async fn next_event(&mut self) -> Option<BackendSessionEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for BackendSessionMonitor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl BackendSessionHandle {
    /// Subscribe before media starts so no terminal session signal can be
    /// missed between preparation and streaming.
    pub async fn start_event_monitor(&self) -> Result<BackendSessionMonitor, BackendSessionError> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (events_tx, events) = mpsc::channel(SESSION_EVENT_CAPACITY);
        let task = tokio::spawn(run_monitor(
            self.connection_for_monitor().clone(),
            self.object_path().clone(),
            self.session_generation(),
            ready_tx,
            events_tx,
        ));
        let mut task = AbortOnDropTask(Some(task));
        let ready = timeout(SESSION_MONITOR_START_TIMEOUT, ready_rx).await;
        match ready {
            Ok(Ok(Ok(()))) => Ok(BackendSessionMonitor {
                events,
                task: Some(task.take()),
            }),
            Ok(Ok(Err(error))) => {
                task.abort().await;
                Err(error)
            }
            Ok(Err(_)) => {
                task.abort().await;
                Err(BackendSessionError::MonitorStopped)
            }
            Err(_) => {
                task.abort().await;
                Err(BackendSessionError::MethodTimeout("StartSessionMonitor"))
            }
        }
    }
}

struct AbortOnDropTask(Option<JoinHandle<()>>);

impl AbortOnDropTask {
    fn take(&mut self) -> JoinHandle<()> {
        self.0.take().expect("session monitor task already taken")
    }

    async fn abort(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for AbortOnDropTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn run_monitor(
    connection: Connection,
    object_path: OwnedObjectPath,
    session_generation: u64,
    ready: oneshot::Sender<Result<(), BackendSessionError>>,
    events: mpsc::Sender<BackendSessionEvent>,
) {
    let proxy = match BackendSession1Proxy::builder(&connection)
        .path(object_path)
        .map_err(BackendSessionError::Protocol)
    {
        Ok(builder) => match builder.build().await.map_err(BackendSessionError::Protocol) {
            Ok(proxy) => proxy,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        },
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut disconnected = match proxy.receive_disconnected().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.send(Err(BackendSessionError::Protocol(error)));
            return;
        }
    };
    let mut fatal = match proxy.receive_fatal_error().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.send(Err(BackendSessionError::Protocol(error)));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    let mut messages = MessageStream::from(&connection);

    loop {
        let event = tokio::select! {
            signal = disconnected.next() => match signal {
                Some(signal) => match signal.args() {
                    Ok(args) => validate_signal(
                        session_generation,
                        *args.session_generation(),
                        args.error_text().clone(),
                        |error_text| BackendSessionEvent::Disconnected {
                            session_generation,
                            error_text,
                        },
                    ),
                    Err(error) => BackendSessionEvent::MonitorFailed {
                        session_generation,
                        error_text: error.to_string(),
                    },
                },
                None => BackendSessionEvent::ConnectionClosed { session_generation },
            },
            signal = fatal.next() => match signal {
                Some(signal) => match signal.args() {
                    Ok(args) => validate_signal(
                        session_generation,
                        *args.session_generation(),
                        args.error_text().clone(),
                        |error_text| BackendSessionEvent::FatalError {
                            session_generation,
                            error_text,
                        },
                    ),
                    Err(error) => BackendSessionEvent::MonitorFailed {
                        session_generation,
                        error_text: error.to_string(),
                    },
                },
                None => BackendSessionEvent::ConnectionClosed { session_generation },
            },
            message = messages.next() => match message {
                None | Some(Err(zbus::Error::InputOutput(_))) => {
                    BackendSessionEvent::ConnectionClosed { session_generation }
                }
                Some(Err(error)) => BackendSessionEvent::MonitorFailed {
                    session_generation,
                    error_text: error.to_string(),
                },
                Some(Ok(_)) => continue,
            },
        };
        let _ = events.send(event).await;
        return;
    }
}

fn validate_signal(
    expected_generation: u64,
    actual_generation: u64,
    error_text: String,
    build: impl FnOnce(String) -> BackendSessionEvent,
) -> BackendSessionEvent {
    if actual_generation != expected_generation {
        return BackendSessionEvent::MonitorFailed {
            session_generation: expected_generation,
            error_text: format!(
                "session signal generation {actual_generation} differs from {expected_generation}"
            ),
        };
    }
    if let Err(error) = validate_error_text(&error_text) {
        return BackendSessionEvent::MonitorFailed {
            session_generation: expected_generation,
            error_text: error.to_string(),
        };
    }
    build(error_text)
}
