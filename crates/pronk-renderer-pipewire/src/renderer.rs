//! Task ownership for one candidate userspace-rendered video generation.

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::time::Duration;

use castkms_renderer::Renderer;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoNodeIdentity, VideoSourceConfig};
use pronk_renderer_worker::{OutputPool, PrivatePool, PrivateProbe, SourceReader};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{active, Registration, Video, VideoEvent};

/// Allocation and transport policy for one candidate renderer generation.
#[derive(Clone, Debug)]
pub struct RendererStreamConfig {
    pub pipewire: VideoSourceConfig,
    pub output_modifier: u64,
    pub private_capacity: NonZeroUsize,
    pub output_capacity: NonZeroUsize,
}

/// Candidate renderer and PipeWire generation owned by one Tokio task.
///
/// Preparation leaves host execution selected. Explicit shutdown joins the
/// task, stops PipeWire, and aborts the unpublished takeover candidate.
pub struct RendererStream<F> {
    handle: Option<StreamHandle<F>>,
    activate: Option<oneshot::Sender<oneshot::Sender<()>>>,
}

/// Activated renderer generation whose endpoint closes when the task stops.
pub struct ActiveRendererStream<F> {
    handle: Option<StreamHandle<F>>,
}

struct StreamHandle<F> {
    identity: VideoNodeIdentity,
    state: watch::Receiver<RendererStreamState>,
    stop: CancellationToken,
    task: Option<JoinHandle<(Option<F>, io::Result<()>)>>,
}

/// Observable lifetime of one renderer stream task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererStreamState {
    Prepared,
    Active,
    Stopped,
    Failed(String),
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
        let (state, receive) = watch::channel(RendererStreamState::Prepared);
        let (started, response) = oneshot::channel();
        let (activate, activation) = oneshot::channel();
        let task = tokio::spawn(run(
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
        ));
        let mut starting = Starting {
            stop: stop.clone(),
            task: Some(task),
            armed: true,
        };
        match response.await {
            Ok(Started::Ready(identity)) => Ok(Self {
                handle: Some(StreamHandle {
                    identity,
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

    /// Activate delegated execution and consume the one-shot candidate handle.
    pub async fn activate(mut self) -> Result<ActiveRendererStream<F>, RendererStreamError<F>> {
        let (acknowledge, acknowledged) = oneshot::channel();
        let activate = self
            .activate
            .take()
            .expect("prepared renderer stream owns activation command");
        let mut handle = self.take_handle();
        if activate.send(acknowledge).is_err() || acknowledged.await.is_err() {
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

    /// Stop transport and close the active renderer endpoint.
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

struct TaskControl {
    stop: CancellationToken,
    started: oneshot::Sender<Started>,
    state: watch::Sender<RendererStreamState>,
    activation: oneshot::Receiver<oneshot::Sender<()>>,
}

async fn run<F: AsFd + Send + 'static>(
    mut renderer: Renderer<F>,
    device: Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    control: TaskControl,
) -> (Option<F>, io::Result<()>) {
    let state = control.state.clone();
    let outcome = run_generation(
        &mut renderer,
        device,
        config,
        remote,
        GenerationControl {
            stop: &control.stop,
            started: control.started,
            state: &control.state,
            activation: control.activation,
        },
    )
    .await;
    state.send_replace(match &outcome.result {
        Ok(()) => RendererStreamState::Stopped,
        Err(error) => RendererStreamState::Failed(error.to_string()),
    });
    let owner = if outcome.activated {
        drop(renderer);
        None
    } else {
        Some(renderer.into_owner())
    };
    (owner, outcome.result)
}

async fn run_generation<F: AsFd>(
    renderer: &mut Renderer<F>,
    device: Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    control: GenerationControl<'_>,
) -> GenerationOutcome {
    let result = prepare_generation(renderer, &device, config, remote, control.started).await;
    let generation = match result {
        Ok(generation) => generation,
        Err(error) => return GenerationOutcome::candidate(Err(error)),
    };
    run_prepared(
        generation,
        device,
        control.stop,
        control.state,
        control.activation,
    )
    .await
}

struct GenerationControl<'a> {
    stop: &'a CancellationToken,
    started: oneshot::Sender<Started>,
    state: &'a watch::Sender<RendererStreamState>,
    activation: oneshot::Receiver<oneshot::Sender<()>>,
}

async fn run_prepared<F: AsFd>(
    generation: PreparedGeneration<'_, F>,
    device: Device,
    stop: &CancellationToken,
    state: &watch::Sender<RendererStreamState>,
    mut activation: oneshot::Receiver<oneshot::Sender<()>>,
) -> GenerationOutcome {
    let PreparedGeneration {
        mut video,
        probe,
        private,
        source_interval,
    } = generation;
    let mut available = VecDeque::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => {
                let result = finish_candidate(video, probe, private, None).await;
                return GenerationOutcome::candidate(result);
            }
            requested = &mut activation => {
                let acknowledge = match requested {
                    Ok(acknowledge) => acknowledge,
                    Err(_) => {
                        let result = finish_candidate(
                            video,
                            probe,
                            private,
                            Some(io::Error::other("renderer activation request was abandoned")),
                        ).await;
                        return GenerationOutcome::candidate(result);
                    }
                };
                return activate_generation(
                    PreparedGeneration {
                        video,
                        probe,
                        private,
                        source_interval,
                    },
                    device,
                    available,
                    ActiveControl {
                        stop,
                        state,
                        acknowledge,
                    },
                ).await;
            }
            event = video.next_event() => match event {
                Ok(VideoEvent::Ignored) => {}
                Ok(VideoEvent::Available { slot }) => {
                    available.push_back(slot);
                }
                Ok(VideoEvent::Released(output)) => {
                    let returned = output.wait().await;
                    let slot = match video.finish_return(returned) {
                        Ok(slot) => slot,
                        Err(error) => {
                            let result = finish_candidate(video, probe, private, Some(error)).await;
                            return GenerationOutcome::candidate(result);
                        }
                    };
                    available.push_back(slot);
                }
                Ok(VideoEvent::Failed { cause, returns }) => {
                    let error = io::Error::other(cause.to_string());
                    for output in returns.into_vec() {
                        let returned = output.wait().await;
                        let _ = video.finish_return(returned);
                    }
                    let result = finish_candidate(video, probe, private, Some(error)).await;
                    return GenerationOutcome::candidate(result);
                }
                Err(error) => {
                    let result = finish_candidate(video, probe, private, Some(error)).await;
                    return GenerationOutcome::candidate(result);
                }
            }
        }
    }
}

async fn activate_generation<F: AsFd>(
    generation: PreparedGeneration<'_, F>,
    device: Device,
    available: VecDeque<usize>,
    control: ActiveControl<'_>,
) -> GenerationOutcome {
    let PreparedGeneration {
        mut video,
        probe,
        private,
        source_interval,
    } = generation;
    let submitted = match probe.submit() {
        Ok(submitted) => submitted,
        Err(error) => {
            let result = finish_video(video, Some(error)).await;
            return GenerationOutcome::candidate(result);
        }
    };
    let active_renderer = match submitted.activate() {
        Ok(active) => active,
        Err(failure) => {
            let result = finish_video(video, Some(failure.into_error())).await;
            return GenerationOutcome::candidate(result);
        }
    };
    let reader = match SourceReader::new(active_renderer, device, private) {
        Ok(reader) => reader,
        Err(failure) => {
            let (_, _, _, error) = failure.into_parts();
            let result = finish_video(video, Some(error)).await;
            return GenerationOutcome::active(result);
        }
    };
    control.state.send_replace(RendererStreamState::Active);
    if control.acknowledge.send(()).is_err() {
        let result = finish_video(
            video,
            Some(io::Error::other(
                "renderer activation acknowledgement was abandoned",
            )),
        )
        .await;
        return GenerationOutcome::active(result);
    }
    let result = active::run(reader, &mut video, available, source_interval, control.stop).await;
    GenerationOutcome::active(finish_video(video, result.err()).await)
}

struct ActiveControl<'a> {
    stop: &'a CancellationToken,
    state: &'a watch::Sender<RendererStreamState>,
    acknowledge: oneshot::Sender<()>,
}

struct PreparedGeneration<'renderer, F: AsFd> {
    video: Video,
    probe: PrivateProbe<'renderer, F>,
    private: PrivatePool,
    source_interval: Duration,
}

async fn prepare_generation<'renderer, F: AsFd>(
    renderer: &'renderer mut Renderer<F>,
    device: &Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    started: oneshot::Sender<Started>,
) -> io::Result<PreparedGeneration<'renderer, F>> {
    let interval_ns = 1_000_000_000u64
        .checked_div(u64::from(config.pipewire.refresh_hz.get()))
        .filter(|interval| *interval != 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "renderer refresh rate exceeds the source clock resolution",
            )
        })?;
    let description = renderer.describe()?;
    let candidate = renderer.begin_takeover(description)?;
    let configuration = candidate.configuration();
    let probe = PrivateProbe::prepare(device, candidate).map_err(|failure| {
        let (candidate, error) = failure.into_parts();
        drop(candidate);
        error
    })?;
    let private = match PrivatePool::new(
        device,
        configuration.width(),
        configuration.height(),
        config.private_capacity,
    ) {
        Ok(private) => private,
        Err(error) => {
            drop(probe);
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
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
        let failure = finish_candidate(
            video,
            probe,
            private,
            Some(io::Error::other("renderer stream setup was abandoned")),
        )
        .await
        .expect_err("explicit renderer setup failure remains terminal");
        return Err(failure);
    }
    Ok(PreparedGeneration {
        video,
        probe,
        private,
        source_interval: Duration::from_nanos(interval_ns),
    })
}

async fn finish_candidate<F: AsFd>(
    video: Video,
    probe: PrivateProbe<'_, F>,
    private: PrivatePool,
    mut failure: Option<io::Error>,
) -> io::Result<()> {
    if let Err(error) = probe.abort() {
        failure.get_or_insert(error);
    }
    drop(private);
    finish_video(video, failure).await
}

async fn finish_video(video: Video, mut failure: Option<io::Error>) -> io::Result<()> {
    let stopped = video.shutdown().await;
    if let Err(error) = stopped.finish().await {
        failure.get_or_insert(error);
    }
    failure.map_or(Ok(()), Err)
}

struct GenerationOutcome {
    activated: bool,
    result: io::Result<()>,
}

impl GenerationOutcome {
    fn candidate(result: io::Result<()>) -> Self {
        Self {
            activated: false,
            result,
        }
    }

    fn active(result: io::Result<()>) -> Self {
        Self {
            activated: true,
            result,
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
            state,
            stop: stop.clone(),
            task: Some(task),
        });
        assert!(stop.is_cancelled());
    }
}
