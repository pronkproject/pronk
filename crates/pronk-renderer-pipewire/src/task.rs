//! Private orchestration for a renderer generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use castkms_renderer::{Renderer, TakeoverCandidate};
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoBufferLayout, VideoNodeIdentity};
use pronk_renderer_worker::{OutputPool, PrivatePool, PrivateProbe, SourceReader};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::active;
use crate::registration::Registration;
use crate::types::{RendererStreamConfig, RendererStreamState};
use crate::video::{Video, VideoEvent};

pub(crate) enum Started {
    Ready {
        identity: VideoNodeIdentity,
        layout: VideoBufferLayout,
    },
    Failed,
}

pub(crate) struct TaskControl {
    pub stop: CancellationToken,
    pub started: oneshot::Sender<Started>,
    pub state: watch::Sender<RendererStreamState>,
    pub activation: oneshot::Receiver<oneshot::Sender<()>>,
}

pub(crate) async fn run<F: AsFd + Send + 'static>(
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
    let generation =
        match prepare_generation(renderer, &device, config, remote, control.started).await {
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
    let probe = match PrivateProbe::prepare(device, candidate) {
        Ok(probe) => probe,
        Err(failure) => {
            let (candidate, error) = failure.into_parts();
            return Err(abort_candidate(candidate, error));
        }
    };
    let private = match PrivatePool::new(
        device,
        configuration.width(),
        configuration.height(),
        config.private_capacity,
    ) {
        Ok(private) => private,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(abort_probe(probe, error));
        }
    };
    let output = match OutputPool::new(
        device,
        configuration.width(),
        configuration.height(),
        config.output_modifier,
        config.output_capacity,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            drop(private);
            return Err(abort_probe(probe, error));
        }
    };
    let registration = match Registration::new(output) {
        Ok(registration) => registration,
        Err(error) => {
            drop(private);
            return Err(abort_probe(probe, error));
        }
    };
    let layout = registration.layout();
    let video = match Video::start(registration, config.pipewire, remote).await {
        Ok(video) => video,
        Err(error) => {
            let _ = started.send(Started::Failed);
            drop(private);
            return Err(abort_probe(probe, error));
        }
    };
    let identity = video.identity().clone();
    if started.send(Started::Ready { identity, layout }).is_err() {
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

fn abort_probe<F: AsFd>(probe: PrivateProbe<'_, F>, error: io::Error) -> io::Error {
    combine_abort(error, probe.abort())
}

fn abort_candidate<F: AsFd>(candidate: TakeoverCandidate<'_, F>, error: io::Error) -> io::Error {
    combine_abort(error, candidate.abort())
}

fn combine_abort(error: io::Error, abort: io::Result<()>) -> io::Error {
    match abort {
        Ok(()) => error,
        Err(abort) => io::Error::new(
            error.kind(),
            format!("{error}; abort renderer takeover: {abort}"),
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::combine_abort;
    use std::io;

    #[test]
    fn takeover_abort_failure_preserves_the_setup_error_class() {
        let error = combine_abort(
            io::Error::new(io::ErrorKind::Unsupported, "unsupported layout"),
            Err(io::Error::from_raw_os_error(nix::libc::EBUSY)),
        );

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            "unsupported layout; abort renderer takeover: Device or resource busy (os error 16)"
        );
    }

    #[test]
    fn successful_takeover_abort_returns_the_original_error() {
        let error = combine_abort(io::Error::other("allocation failed"), Ok(()));
        assert_eq!(error.to_string(), "allocation failed");
    }
}
