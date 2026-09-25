use std::collections::BTreeMap;

use pronk_backend_host::{
    BackendHandle, BackendShutdownReport, BackendSupervisor, BackendSupervisorError,
    BackendSupervisorEvent,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::{ManagerShutdownReport, MANAGER_SHUTDOWN_TIMEOUT};

#[derive(Debug)]
pub(super) enum BackendWorkerMessage {
    Event {
        backend_id: String,
        event: BackendSupervisorEvent,
    },
    Stopped {
        backend_id: String,
        error: String,
    },
}

pub(super) struct BackendWorker {
    pub(super) backend_id: String,
    pub(super) handle: BackendHandle,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<BackendShutdownReport, BackendSupervisorError>>,
}

impl BackendWorker {
    pub(super) fn spawn(
        backend_id: String,
        supervisor: BackendSupervisor,
        events: mpsc::Sender<BackendWorkerMessage>,
    ) -> Self {
        let handle = supervisor.handle();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task_backend_id = backend_id.clone();
        let task = tokio::spawn(run_backend_worker(
            task_backend_id,
            supervisor,
            events,
            shutdown_rx,
        ));
        Self {
            backend_id,
            handle,
            shutdown: Some(shutdown_tx),
            task,
        }
    }
}

async fn run_backend_worker(
    backend_id: String,
    mut supervisor: BackendSupervisor,
    events: mpsc::Sender<BackendWorkerMessage>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<BackendShutdownReport, BackendSupervisorError> {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => return supervisor.shutdown().await,
            event = supervisor.next_event() => match event {
                Some(event) => {
                    let message = BackendWorkerMessage::Event {
                        backend_id: backend_id.clone(),
                        event,
                    };
                    tokio::select! {
                        biased;
                        _ = &mut shutdown => return supervisor.shutdown().await,
                        result = events.send(message) => {
                            if result.is_err() {
                                return supervisor.shutdown().await;
                            }
                        }
                    }
                }
                None => {
                    let _ = events.send(BackendWorkerMessage::Stopped {
                        backend_id,
                        error: "backend supervisor event stream closed".into(),
                    }).await;
                    return Err(BackendSupervisorError::SupervisorStopped);
                }
            },
        }
    }
}

pub(super) async fn shutdown_workers(mut workers: Vec<BackendWorker>) -> ManagerShutdownReport {
    for worker in &mut workers {
        if let Some(shutdown) = worker.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    let mut backend_reports = BTreeMap::new();
    let mut errors = BTreeMap::new();
    let wait = async {
        for worker in &mut workers {
            match (&mut worker.task).await {
                Ok(Ok(report)) => {
                    backend_reports.insert(worker.backend_id.clone(), report);
                }
                Ok(Err(error)) => {
                    errors.insert(worker.backend_id.clone(), error.to_string());
                }
                Err(error) => {
                    errors.insert(worker.backend_id.clone(), error.to_string());
                }
            }
        }
    };
    if timeout(MANAGER_SHUTDOWN_TIMEOUT, wait).await.is_err() {
        for worker in &workers {
            if !worker.task.is_finished() {
                worker.task.abort();
                errors.insert(worker.backend_id.clone(), "shutdown timed out".into());
            }
        }
    }
    ManagerShutdownReport {
        backend_reports,
        errors,
    }
}
