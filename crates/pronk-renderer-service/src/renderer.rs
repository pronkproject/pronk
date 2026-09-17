//! Task ownership for one userspace-rendered output generation.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::Renderer;
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::{Device, RenderNodeIdentity};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::task::{run, Started, TaskControl};
use crate::types::{RendererStreamConfig, RendererStreamState};

/// Published renderer offer owned by one dedicated native-work thread.
///
/// Publication does not select the offer. Explicit shutdown joins the task and
/// closes the renderer endpoint after its native work ends.
pub struct RendererStream<F> {
    handle: Option<StreamHandle<F>>,
}

/// Activated renderer generation whose descriptor owner is consumed when the task stops.
pub struct ActiveRendererStream<F> {
    handle: Option<StreamHandle<F>>,
}

struct StreamHandle<F> {
    output: Extent,
    render_node: RenderNodeIdentity,
    state: watch::Receiver<RendererStreamState>,
    stop: CancellationToken,
    task: Option<JoinHandle<(Option<F>, io::Result<()>)>>,
}

impl<F: AsFd + Send + 'static> RendererStream<F> {
    /// Prepare private GPU work and publish a selectable renderer offer.
    pub async fn prepare(
        renderer: Renderer<F>,
        device: Device,
        config: RendererStreamConfig,
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
        let render_node = device.render_node_identity();
        let stop = CancellationToken::new();
        let (state, receive) = watch::channel(RendererStreamState::Starting);
        let (started, response) = oneshot::channel();
        let input = (
            renderer,
            device,
            config,
            TaskControl {
                stop: stop.clone(),
                started,
                state,
            },
        );
        let task = match crate::native_task::spawn(input, |(renderer, device, config, control)| {
            run(renderer, device, config, control)
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
            Ok(Started::Ready { output }) => Ok(Self {
                handle: Some(StreamHandle {
                    output,
                    render_node,
                    state: receive,
                    stop,
                    task: Some(starting.take_task()),
                }),
            }),
            Ok(Started::Failed) => Err(starting.join().await),
            Err(_) => Err(starting.join().await),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<RendererStreamState> {
        self.handle().state.clone()
    }

    pub fn state(&self) -> RendererStreamState {
        self.handle().state.borrow().clone()
    }

    pub fn output(&self) -> Extent {
        self.handle().output
    }

    pub fn render_node_identity(&self) -> RenderNodeIdentity {
        self.handle().render_node
    }

    /// Enter the surrounding media session's active state.
    ///
    /// The renderer offer was already published during preparation; KMS selects
    /// it independently through the generic constraints interface.
    pub async fn activate(
        mut self,
        cancellation: CancellationToken,
    ) -> Result<ActiveRendererStream<F>, RendererStreamError<F>> {
        if cancellation.is_cancelled() {
            let mut handle = self.take_handle();
            handle.stop.cancel();
            return Err(join_cancelled(
                handle.take_task(),
                "renderer media activation was cancelled",
            )
            .await);
        }
        Ok(ActiveRendererStream {
            handle: Some(self.take_handle()),
        })
    }

    /// Stop native work and close the published renderer endpoint.
    pub async fn shutdown(mut self) -> Result<(), RendererStreamError<F>> {
        let mut handle = self.take_handle();
        handle.stop.cancel();
        join_closed(handle.take_task()).await
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
                "output",
                &self
                    .handle
                    .as_ref()
                    .expect("live renderer stream owns its handle")
                    .output,
            )
            .finish_non_exhaustive()
    }
}

impl<F: AsFd + Send + 'static> ActiveRendererStream<F> {
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

    pub fn output(&self) -> Extent {
        self.handle
            .as_ref()
            .expect("live active renderer stream owns its handle")
            .output
    }

    /// Stop native work and release the task's active renderer descriptor.
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

    fn output() -> Extent {
        Extent::new(1920, 1080).unwrap()
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
        let (_, state) = watch::channel(RendererStreamState::Starting);
        drop(StreamHandle {
            output: output(),
            render_node: RenderNodeIdentity {
                major: 226,
                minor: 128,
            },
            state,
            stop: stop.clone(),
            task: Some(task),
        });
        assert!(stop.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_media_activation_stops_the_published_stream() {
        let stop = CancellationToken::new();
        let task = waiting_start(stop.clone());
        let (_, state) = watch::channel(RendererStreamState::Starting);
        let stream = RendererStream {
            handle: Some(StreamHandle {
                output: output(),
                render_node: RenderNodeIdentity {
                    major: 226,
                    minor: 128,
                },
                state,
                stop,
                task: Some(task),
            }),
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
    }

    #[tokio::test]
    async fn cancelled_activation_retains_cleanup_failure() {
        let task = tokio::spawn(async {
            (
                Some(7),
                Err(io::Error::other("renderer offer cleanup failed")),
            )
        });

        let error = join_cancelled(task, "renderer media activation was cancelled").await;
        let (owner, error) = error.into_parts();
        assert_eq!(owner, Some(7));
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error.to_string(),
            "renderer media activation was cancelled; renderer cleanup failed: renderer offer cleanup failed"
        );
    }

    #[tokio::test]
    async fn cancelled_preparation_retains_cleanup_failure() {
        let stop = CancellationToken::new();
        let task = tokio::spawn(async {
            (
                Some(7),
                Err(io::Error::other("renderer offer cleanup failed")),
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
            "renderer stream preparation was cancelled; renderer cleanup failed: renderer offer cleanup failed"
        );
    }
}
