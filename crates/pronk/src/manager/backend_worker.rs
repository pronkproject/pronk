use std::collections::BTreeMap;

use pronk_backend_host::{
    BackendHandle, BackendShutdownReport, BackendSupervisor, BackendSupervisorError,
    BackendSupervisorEvent,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};

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

    let deadline = Instant::now() + MANAGER_SHUTDOWN_TIMEOUT;
    let tasks = workers
        .into_iter()
        .map(|worker| (worker.backend_id, worker.task))
        .collect();
    collect_worker_results(tasks, deadline).await
}

type WorkerTask = JoinHandle<Result<BackendShutdownReport, BackendSupervisorError>>;

async fn collect_worker_results(
    mut tasks: Vec<(String, WorkerTask)>,
    deadline: Instant,
) -> ManagerShutdownReport {
    let mut backend_reports = BTreeMap::new();
    let mut errors = BTreeMap::new();
    let mut next_worker = 0;
    while let Some((backend_id, task)) = tasks.get_mut(next_worker) {
        let Ok(result) = timeout_at(deadline, task).await else {
            break;
        };
        record_worker_result(backend_id, result, &mut backend_reports, &mut errors);
        next_worker += 1;
    }
    for (backend_id, task) in &mut tasks[next_worker..] {
        if task.is_finished() {
            record_worker_result(backend_id, task.await, &mut backend_reports, &mut errors);
        } else {
            task.abort();
            errors.insert(backend_id.clone(), "shutdown timed out".into());
        }
    }
    ManagerShutdownReport {
        backend_reports,
        errors,
    }
}

fn record_worker_result(
    backend_id: &str,
    result: Result<Result<BackendShutdownReport, BackendSupervisorError>, tokio::task::JoinError>,
    backend_reports: &mut BTreeMap<String, BackendShutdownReport>,
    errors: &mut BTreeMap<String, String>,
) {
    match result {
        Ok(Ok(report)) => {
            backend_reports.insert(backend_id.into(), report);
        }
        Ok(Err(error)) => {
            errors.insert(backend_id.into(), error.to_string());
        }
        Err(error) => {
            errors.insert(backend_id.into(), error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_worker_reports_survive_an_earlier_timeout() {
        let stalled = tokio::spawn(std::future::pending());
        let completed = tokio::spawn(async {
            Ok(BackendShutdownReport {
                last_connection_generation: None,
                graceful: true,
                errors: Vec::new(),
            })
        });
        let report = collect_worker_results(
            vec![("stalled".into(), stalled), ("completed".into(), completed)],
            Instant::now() + std::time::Duration::from_millis(20),
        )
        .await;
        assert_eq!(
            report.errors.get("stalled").map(String::as_str),
            Some("shutdown timed out")
        );
        assert!(report.backend_reports.contains_key("completed"));
    }
}
