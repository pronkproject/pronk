//! Task ownership for one candidate userspace-rendered video generation.

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;

use castkms_renderer::Renderer;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoNodeIdentity, VideoSourceConfig};
use pronk_renderer_worker::{OutputPool, PrivateProbe};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{Registration, Video};

/// Allocation and transport policy for one candidate renderer generation.
#[derive(Clone, Debug)]
pub struct RendererStreamConfig {
    pub pipewire: VideoSourceConfig,
    pub output_modifier: u64,
    pub output_capacity: NonZeroUsize,
}

/// Candidate renderer and PipeWire generation owned by one Tokio task.
///
/// Preparation leaves host execution selected. Explicit shutdown joins the
/// task, stops PipeWire, and aborts the unpublished takeover candidate.
pub struct RendererStream<F> {
    identity: VideoNodeIdentity,
    stop: CancellationToken,
    task: Option<JoinHandle<(F, io::Result<()>)>>,
}

impl<F: AsFd + Send + 'static> RendererStream<F> {
    /// Prepare private GPU work and output transport without activating takeover.
    pub async fn prepare(
        renderer: Renderer<F>,
        device: Device,
        config: RendererStreamConfig,
        remote: PipeWireRemote,
    ) -> Result<Self, RendererStreamError<F>> {
        let stop = CancellationToken::new();
        let (started, response) = oneshot::channel();
        let task = tokio::spawn(run(renderer, device, config, remote, stop.clone(), started));
        let mut starting = Starting {
            stop: stop.clone(),
            task: Some(task),
            armed: true,
        };
        match response.await {
            Ok(Started::Ready(identity)) => Ok(Self {
                identity,
                stop,
                task: Some(starting.take_task()),
            }),
            Ok(Started::Failed) => Err(starting.join().await),
            Err(_) => Err(starting.join().await),
        }
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        &self.identity
    }

    /// Stop transport, abort the candidate, and return the renderer owner.
    pub async fn shutdown(mut self) -> Result<F, RendererStreamError<F>> {
        self.stop.cancel();
        join(
            self.task
                .take()
                .expect("live renderer stream owns its task"),
        )
        .await
    }
}

impl<F> Drop for RendererStream<F> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl<F> std::fmt::Debug for RendererStream<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererStream")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
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
    task: Option<JoinHandle<(F, io::Result<()>)>>,
    armed: bool,
}

impl<F> Starting<F> {
    fn take_task(&mut self) -> JoinHandle<(F, io::Result<()>)> {
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
            Ok((owner, Err(error))) => RendererStreamError {
                owner: Some(owner),
                error,
            },
            Ok((owner, Ok(()))) => RendererStreamError {
                owner: Some(owner),
                error: io::Error::other("renderer stream stopped during setup"),
            },
            Err(error) => RendererStreamError {
                owner: None,
                error: io::Error::other(format!("join renderer stream setup: {error}")),
            },
        }
    }
}

impl<F> Drop for Starting<F> {
    fn drop(&mut self) {
        if self.armed {
            self.stop.cancel();
        }
    }
}

enum Started {
    Ready(VideoNodeIdentity),
    Failed,
}

async fn run<F: AsFd + Send + 'static>(
    mut renderer: Renderer<F>,
    device: Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    stop: CancellationToken,
    started: oneshot::Sender<Started>,
) -> (F, io::Result<()>) {
    let result = run_generation(&mut renderer, &device, config, remote, &stop, started).await;
    (renderer.into_owner(), result)
}

async fn run_generation<F: AsFd>(
    renderer: &mut Renderer<F>,
    device: &Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    stop: &CancellationToken,
    started: oneshot::Sender<Started>,
) -> io::Result<()> {
    let description = renderer.describe()?;
    let candidate = renderer.begin_takeover(description)?;
    let configuration = candidate.configuration();
    let probe = PrivateProbe::prepare(device, candidate).map_err(|failure| {
        let (candidate, error) = failure.into_parts();
        drop(candidate);
        error
    })?;
    let output = OutputPool::new(
        device,
        configuration.width(),
        configuration.height(),
        config.output_modifier,
        config.output_capacity,
    )
    .await?;
    let registration = Registration::new(output)?;
    let video = match Video::start(registration, config.pipewire, remote).await {
        Ok(video) => video,
        Err(error) => {
            drop(probe);
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let identity = video.identity().clone();
    if started.send(Started::Ready(identity)).is_err() {
        return finish(
            video,
            probe,
            Some(io::Error::other("renderer stream setup was abandoned")),
        )
        .await;
    }
    stop.cancelled().await;
    finish(video, probe, None).await
}

async fn finish<F: AsFd>(
    video: Video,
    probe: PrivateProbe<'_, F>,
    mut failure: Option<io::Error>,
) -> io::Result<()> {
    let stopped = video.shutdown().await;
    if let Err(error) = stopped.finish().await {
        failure.get_or_insert(error);
    }
    if let Err(error) = probe.abort() {
        failure.get_or_insert(error);
    }
    failure.map_or(Ok(()), Err)
}

async fn join<F>(task: JoinHandle<(F, io::Result<()>)>) -> Result<F, RendererStreamError<F>> {
    match task.await {
        Ok((owner, Ok(()))) => Ok(owner),
        Ok((owner, Err(error))) => Err(RendererStreamError {
            owner: Some(owner),
            error,
        }),
        Err(error) => Err(RendererStreamError {
            owner: None,
            error: io::Error::other(format!("join renderer stream: {error}")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn assert_send<T: Send>() {}

    fn waiting_start(stop: CancellationToken) -> JoinHandle<(std::fs::File, io::Result<()>)> {
        tokio::spawn(async move {
            stop.cancelled().await;
            (std::fs::File::open("/dev/null").unwrap(), Ok(()))
        })
    }

    #[test]
    fn renderer_stream_ownership_can_cross_tasks() {
        assert_send::<RendererStream<std::fs::File>>();
        assert_send::<RendererStreamError<std::fs::File>>();
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
}
