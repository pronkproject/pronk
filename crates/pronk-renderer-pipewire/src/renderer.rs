//! Task ownership for one candidate userspace-rendered video generation.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{ProfileRegistration, Renderer};
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoBufferLayout, VideoNodeIdentity};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::task::{run, Started, TaskControl};
use crate::types::{RendererStreamConfig, RendererStreamState};

/// Candidate renderer and PipeWire generation owned by one dedicated thread.
///
/// Preparation leaves host execution selected. Explicit shutdown joins the
/// task, stops PipeWire, and aborts the unpublished takeover candidate.
pub struct RendererStream<F> {
    handle: Option<StreamHandle<F>>,
    activate: Option<oneshot::Sender<oneshot::Sender<()>>>,
}

/// Activated renderer generation whose descriptor owner is consumed when the task stops.
pub struct ActiveRendererStream<F> {
    handle: Option<StreamHandle<F>>,
}

struct StreamHandle<F> {
    identity: VideoNodeIdentity,
    layout: VideoBufferLayout,
    registration: ProfileRegistration,
    state: watch::Receiver<RendererStreamState>,
    stop: CancellationToken,
    task: Option<JoinHandle<(Option<F>, io::Result<()>)>>,
}

impl<F: AsFd + Send + 'static> RendererStream<F> {
    /// Prepare private GPU work and output transport without activating takeover.
    pub async fn prepare(
        renderer: Renderer<F>,
        device: Device,
        config: RendererStreamConfig,
        remote: PipeWireRemote,
        cancellation: CancellationToken,
    ) -> Result<Self, RendererStreamError<F>> {
        if cancellation.is_cancelled() {
            return Err(RendererStreamError {
                owner: Some(renderer.into_owner()),
                error: io::Error::new(
                    io::ErrorKind::Interrupted,
                    "renderer stream preparation was cancelled",
                ),
            });
        }
        let stop = CancellationToken::new();
        let (state, receive) = watch::channel(RendererStreamState::Prepared);
        let (started, response) = oneshot::channel();
        let (activate, activation) = oneshot::channel();
        let input = (
            renderer,
            device,
            config,
            remote,
            TaskControl {
                stop: stop.clone(),
                started,
                state,
                activation,
            },
        );
        let task =
            match crate::native_task::spawn(input, |(renderer, device, config, remote, control)| {
                run(renderer, device, config, remote, control)
            }) {
                Ok(task) => task,
                Err(((renderer, ..), error)) => {
                    return Err(RendererStreamError {
                        owner: Some(renderer.into_owner()),
                        error,
                    });
                }
            };
        let mut starting = Starting {
            stop: stop.clone(),
            task: Some(task),
            armed: true,
        };
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(starting.cancel().await);
            }
            response = response => response,
        };
        match response {
            Ok(Started::Ready {
                identity,
                layout,
                registration,
            }) => Ok(Self {
                handle: Some(StreamHandle {
                    identity,
                    layout,
                    registration,
                    state: receive,
                    stop,
                    task: Some(starting.take_task()),
                }),
                activate: Some(activate),
            }),
            Ok(Started::Failed) => Err(starting.join().await),
            Err(_) => Err(starting.join().await),
        }
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        &self.handle().identity
    }

    pub fn subscribe(&self) -> watch::Receiver<RendererStreamState> {
        self.handle().state.clone()
    }

    pub fn state(&self) -> RendererStreamState {
        self.handle().state.borrow().clone()
    }

    pub fn layout(&self) -> VideoBufferLayout {
        self.handle().layout
    }

    /// Return the transition that the KMS client must install before activation.
    pub fn profile_registration(&self) -> ProfileRegistration {
        self.handle().registration
    }

    /// Activate delegated execution and consume the one-shot candidate handle.
    pub async fn activate(
        mut self,
        cancellation: CancellationToken,
    ) -> Result<ActiveRendererStream<F>, RendererStreamError<F>> {
        if cancellation.is_cancelled() {
            let mut handle = self.take_handle();
            return Err(cancel_activation(&mut handle).await);
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        let activate = self
            .activate
            .take()
            .expect("prepared renderer stream owns activation command");
        let mut handle = self.take_handle();
        if activate.send(acknowledge).is_err() {
            return Err(join_failure(handle.take_task()).await);
        }
        let acknowledged = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(cancel_activation(&mut handle).await);
            }
            acknowledged = acknowledged => acknowledged,
        };
        if acknowledged.is_err() {
            return Err(join_failure(handle.take_task()).await);
        }
        Ok(ActiveRendererStream {
            handle: Some(handle),
        })
    }

    /// Stop transport, abort the candidate, and return the renderer owner.
    pub async fn shutdown(mut self) -> Result<F, RendererStreamError<F>> {
        let mut handle = self.take_handle();
        handle.stop.cancel();
        join_owner(handle.take_task()).await
    }

    fn handle(&self) -> &StreamHandle<F> {
        self.handle
            .as_ref()
            .expect("live renderer stream owns its handle")
    }

    fn take_handle(&mut self) -> StreamHandle<F> {
        self.handle
            .take()
            .expect("live renderer stream owns its handle")
    }
}

async fn cancel_activation<F>(handle: &mut StreamHandle<F>) -> RendererStreamError<F> {
    handle.stop.cancel();
    join_cancelled(
        handle.take_task(),
        "renderer stream activation was cancelled",
    )
    .await
}

impl<F> Drop for RendererStream<F> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.stop.cancel();
        }
    }
}

impl<F> std::fmt::Debug for RendererStream<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererStream")
            .field(
                "identity",
                &self
                    .handle
                    .as_ref()
                    .expect("live renderer stream owns its handle")
                    .identity,
            )
            .finish_non_exhaustive()
    }
}

impl<F: AsFd + Send + 'static> ActiveRendererStream<F> {
    pub fn identity(&self) -> &VideoNodeIdentity {
        &self
            .handle
            .as_ref()
            .expect("live active renderer stream owns its handle")
            .identity
    }

    pub fn subscribe(&self) -> watch::Receiver<RendererStreamState> {
        self.handle
            .as_ref()
            .expect("live active renderer stream owns its handle")
            .state
            .clone()
    }

    pub fn state(&self) -> RendererStreamState {
        self.handle
            .as_ref()
            .expect("live active renderer stream owns its handle")
            .state
            .borrow()
            .clone()
    }

    pub fn layout(&self) -> VideoBufferLayout {
        self.handle
            .as_ref()
            .expect("live active renderer stream owns its handle")
            .layout
    }

    /// Stop transport and release the task's active renderer descriptor.
    pub async fn shutdown(mut self) -> Result<(), RendererStreamError<F>> {
        let mut handle = self
            .handle
            .take()
            .expect("live active renderer stream owns its handle");
        handle.stop.cancel();
        join_closed(handle.take_task()).await
    }
}

impl<F> Drop for ActiveRendererStream<F> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.stop.cancel();
        }
    }
}

impl<F> StreamHandle<F> {
    fn take_task(&mut self) -> JoinHandle<(Option<F>, io::Result<()>)> {
        self.task
            .take()
            .expect("live renderer stream owns its task")
    }
}

impl<F> Drop for StreamHandle<F> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Failed renderer-stream setup or shutdown with any recovered descriptor owner.
pub struct RendererStreamError<F> {
    owner: Option<F>,
    error: io::Error,
}

impl<F> RendererStreamError<F> {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (Option<F>, io::Error) {
        (self.owner, self.error)
    }
}

impl<F> std::fmt::Debug for RendererStreamError<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererStreamError")
            .field("owner_recovered", &self.owner.is_some())
            .field("error", &self.error)
            .finish()
    }
}

impl<F> std::fmt::Display for RendererStreamError<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl<F> std::error::Error for RendererStreamError<F> {}

struct Starting<F> {
    stop: CancellationToken,
    task: Option<JoinHandle<(Option<F>, io::Result<()>)>>,
    armed: bool,
}

impl<F> Starting<F> {
    fn take_task(&mut self) -> JoinHandle<(Option<F>, io::Result<()>)> {
        self.armed = false;
        self.task
            .take()
            .expect("starting renderer stream owns task")
    }

    async fn join(mut self) -> RendererStreamError<F> {
        self.stop.cancel();
        let task = self
            .task
            .take()
            .expect("starting renderer stream owns task");
        match task.await {
            Ok((owner, Err(error))) => RendererStreamError { owner, error },
            Ok((owner, Ok(()))) => RendererStreamError {
                owner,
                error: io::Error::other("renderer stream stopped during setup"),
            },
            Err(error) => RendererStreamError {
                owner: None,
                error: io::Error::other(format!("join renderer stream setup: {error}")),
            },
        }
    }

    async fn cancel(mut self) -> RendererStreamError<F> {
        self.stop.cancel();
        let task = self
            .task
            .take()
            .expect("starting renderer stream owns task");
        join_cancelled(task, "renderer stream preparation was cancelled").await
    }
}

impl<F> Drop for Starting<F> {
    fn drop(&mut self) {
        if self.armed {
            self.stop.cancel();
        }
    }
}

async fn join_owner<F>(
    task: JoinHandle<(Option<F>, io::Result<()>)>,
) -> Result<F, RendererStreamError<F>> {
    match task.await {
        Ok((Some(owner), Ok(()))) => Ok(owner),
        Ok((owner, Err(error))) => Err(RendererStreamError { owner, error }),
        Ok((None, Ok(()))) => Err(RendererStreamError {
            owner: None,
            error: io::Error::other("candidate renderer closed its descriptor"),
        }),
        Err(error) => Err(RendererStreamError {
            owner: None,
            error: io::Error::other(format!("join renderer stream: {error}")),
        }),
    }
}

async fn join_failure<F>(task: JoinHandle<(Option<F>, io::Result<()>)>) -> RendererStreamError<F> {
    match task.await {
        Ok((owner, Err(error))) => RendererStreamError { owner, error },
        Ok((owner, Ok(()))) => RendererStreamError {
            owner,
            error: io::Error::other("renderer stopped before activation"),
        },
        Err(error) => RendererStreamError {
            owner: None,
            error: join_error(error),
        },
    }
}

async fn join_cancelled<F>(
    task: JoinHandle<(Option<F>, io::Result<()>)>,
    message: &'static str,
) -> RendererStreamError<F> {
    match task.await {
        Ok((owner, Ok(()))) => RendererStreamError {
            owner,
            error: io::Error::new(io::ErrorKind::Interrupted, message),
        },
        Ok((owner, Err(error))) => RendererStreamError {
            owner,
            error: io::Error::new(
                io::ErrorKind::Interrupted,
                format!("{message}; renderer cleanup failed: {error}"),
            ),
        },
        Err(error) => RendererStreamError {
            owner: None,
            error: io::Error::new(io::ErrorKind::Interrupted, format!("{message}; {error}")),
        },
    }
}

async fn join_closed<F>(
    task: JoinHandle<(Option<F>, io::Result<()>)>,
) -> Result<(), RendererStreamError<F>> {
    match task.await {
        Ok((None, Ok(()))) => Ok(()),
        Ok((owner, Err(error))) => Err(RendererStreamError { owner, error }),
        Ok((owner @ Some(_), Ok(()))) => Err(RendererStreamError {
            owner,
            error: io::Error::other("active renderer returned an open descriptor"),
        }),
        Err(error) => Err(RendererStreamError {
            owner: None,
            error: join_error(error),
        }),
    }
}

fn join_error(error: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("join renderer stream: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};
    use std::time::Duration;

    fn assert_send<T: Send>() {}

    fn waiting_start(
        stop: CancellationToken,
    ) -> JoinHandle<(Option<std::fs::File>, io::Result<()>)> {
        tokio::spawn(async move {
            stop.cancelled().await;
            (Some(std::fs::File::open("/dev/null").unwrap()), Ok(()))
        })
    }

    fn identity() -> VideoNodeIdentity {
        VideoNodeIdentity {
            node_name: "renderer-test".into(),
            object_id: NonZeroU32::new(1).unwrap(),
            object_serial: NonZeroU64::new(2).unwrap(),
            media_generation: NonZeroU64::new(3).unwrap(),
        }
    }

    fn profile_registration() -> ProfileRegistration {
        ProfileRegistration::from_values(4, 5, 6).unwrap()
    }

    #[test]
    fn renderer_stream_ownership_can_cross_tasks() {
        assert_send::<RendererStream<std::fs::File>>();
        assert_send::<ActiveRendererStream<std::fs::File>>();
        assert_send::<RendererStreamError<std::fs::File>>();
        assert_send::<RendererStreamState>();
    }

    #[tokio::test]
    async fn abandoned_setup_signals_worker_cancellation() {
        let stop = CancellationToken::new();
        let task = waiting_start(stop.clone());
        drop(Starting {
            stop: stop.clone(),
            task: Some(task),
            armed: true,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !stop.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn completed_setup_leaves_its_worker_running() {
        let stop = CancellationToken::new();
        let task = waiting_start(stop.clone());
        let mut starting = Starting {
            stop: stop.clone(),
            task: Some(task),
            armed: true,
        };
        let task = starting.take_task();
        drop(starting);
        assert!(!stop.is_cancelled());
        stop.cancel();
        let (_, result) = task.await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn abandoned_stream_handle_signals_worker_cancellation() {
        let stop = CancellationToken::new();
        let task = waiting_start(stop.clone());
        let (_, state) = watch::channel(RendererStreamState::Prepared);
        drop(StreamHandle {
            identity: identity(),
            layout: VideoBufferLayout {
                format: pronk_pipewire::VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(1).unwrap(),
                height: NonZeroU32::new(1).unwrap(),
                pitch: NonZeroU32::new(4).unwrap(),
                size: NonZeroU64::new(4).unwrap(),
                storage: pronk_pipewire::VideoBufferStorage::MappableLinear,
            },
            registration: profile_registration(),
            state,
            stop: stop.clone(),
            task: Some(task),
        });
        assert!(stop.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_activation_does_not_send_the_takeover_command() {
        let stop = CancellationToken::new();
        let task = waiting_start(stop.clone());
        let (_, state) = watch::channel(RendererStreamState::Prepared);
        let (activate, activation) = oneshot::channel();
        let stream = RendererStream {
            handle: Some(StreamHandle {
                identity: identity(),
                layout: VideoBufferLayout {
                    format: pronk_pipewire::VideoPixelFormat::Xrgb8888,
                    width: NonZeroU32::new(1).unwrap(),
                    height: NonZeroU32::new(1).unwrap(),
                    pitch: NonZeroU32::new(4).unwrap(),
                    size: NonZeroU64::new(4).unwrap(),
                    storage: pronk_pipewire::VideoBufferStorage::MappableLinear,
                },
                registration: profile_registration(),
                state,
                stop,
                task: Some(task),
            }),
            activate: Some(activate),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = match stream.activate(cancellation).await {
            Ok(_) => panic!("cancelled activation unexpectedly succeeded"),
            Err(error) => error,
        };
        let (owner, error) = error.into_parts();
        assert!(owner.is_some());
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(activation.await.is_err());
    }

    #[tokio::test]
    async fn cancelled_activation_retains_cleanup_failure() {
        let task = tokio::spawn(async {
            (
                Some(7),
                Err(io::Error::other("renderer candidate abort failed")),
            )
        });

        let error = join_cancelled(task, "renderer stream activation was cancelled").await;
        let (owner, error) = error.into_parts();
        assert_eq!(owner, Some(7));
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error.to_string(),
            "renderer stream activation was cancelled; renderer cleanup failed: renderer candidate abort failed"
        );
    }

    #[tokio::test]
    async fn cancelled_preparation_retains_cleanup_failure() {
        let stop = CancellationToken::new();
        let task = tokio::spawn(async {
            (
                Some(7),
                Err(io::Error::other("renderer candidate abort failed")),
            )
        });
        let starting = Starting {
            stop: stop.clone(),
            task: Some(task),
            armed: true,
        };

        let error = starting.cancel().await;
        let (owner, error) = error.into_parts();
        assert!(stop.is_cancelled());
        assert_eq!(owner, Some(7));
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error.to_string(),
            "renderer stream preparation was cancelled; renderer cleanup failed: renderer candidate abort failed"
        );
    }
}
