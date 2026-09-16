//! Private orchestration for a renderer generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use castkms_renderer::{ProfileRegistration, RegisteredCandidate, Renderer, TakeoverCandidate};
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoBufferLayout, VideoNodeIdentity};
use pronk_renderer_worker::{
    OutputPool, PreparedSceneImages, PrimarySceneProfile, PrivateProbe, SceneReader,
    SceneStorageProfile,
};
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
        registration: ProfileRegistration,
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
    run_prepared(generation, control.stop, control.state, control.activation).await
}

struct GenerationControl<'a> {
    stop: &'a CancellationToken,
    started: oneshot::Sender<Started>,
    state: &'a watch::Sender<RendererStreamState>,
    activation: oneshot::Receiver<oneshot::Sender<()>>,
}

async fn run_prepared<F: AsFd>(
    generation: PreparedGeneration<'_, F>,
    stop: &CancellationToken,
    state: &watch::Sender<RendererStreamState>,
    mut activation: oneshot::Receiver<oneshot::Sender<()>>,
) -> GenerationOutcome {
    let PreparedGeneration {
        mut video,
        probe,
        storage,
        scene_pool,
        source_interval,
        scene_images,
    } = generation;
    let mut available = VecDeque::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => {
                let result = finish_candidate(video, probe, scene_pool, None).await;
                return GenerationOutcome::candidate(result);
            }
            requested = &mut activation => {
                let acknowledge = match requested {
                    Ok(acknowledge) => acknowledge,
                    Err(_) => {
                        let result = finish_candidate(
                            video,
                            probe,
                            scene_pool,
                            Some(io::Error::other("renderer activation request was abandoned")),
                        ).await;
                        return GenerationOutcome::candidate(result);
                    }
                };
                return activate_generation(
                    PreparedGeneration {
                        video,
                        probe,
                        storage,
                        scene_pool,
                        source_interval,
                        scene_images,
                    },
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
                            let result = finish_candidate(video, probe, scene_pool, Some(error)).await;
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
                    let result = finish_candidate(video, probe, scene_pool, Some(error)).await;
                    return GenerationOutcome::candidate(result);
                }
                Err(error) => {
                    let result = finish_candidate(video, probe, scene_pool, Some(error)).await;
                    return GenerationOutcome::candidate(result);
                }
            }
        }
    }
}

async fn activate_generation<F: AsFd>(
    generation: PreparedGeneration<'_, F>,
    available: VecDeque<usize>,
    control: ActiveControl<'_>,
) -> GenerationOutcome {
    let PreparedGeneration {
        mut video,
        probe,
        storage,
        scene_pool,
        source_interval,
        scene_images,
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
    let reader = match SceneReader::new(active_renderer, storage, scene_pool, scene_images) {
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
    let result =
        active::run_complete_scenes(reader, &mut video, available, source_interval, control.stop)
            .await;
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
    storage: SceneStorageProfile,
    scene_pool: pronk_renderer_worker::ScenePool,
    source_interval: Duration,
    scene_images: PreparedSceneImages,
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
    let output_extent = Extent::new(configuration.width().get(), configuration.height().get())
        .expect("candidate output dimensions are nonzero");
    let profile = match PrimarySceneProfile::discover(device, output_extent) {
        Ok(profile) => profile,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(abort_candidate(candidate, error));
        }
    };
    let scene_pool = match profile.create_pool(config.private_capacity, config.private_capacity) {
        Ok(scene_pool) => scene_pool,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(abort_candidate(candidate, error));
        }
    };
    let scene_images = match PreparedSceneImages::new(
        device,
        configuration.width(),
        configuration.height(),
        config.output_modifier,
        config.private_capacity,
    ) {
        Ok(images) => images,
        Err(error) => {
            let _ = started.send(Started::Failed);
            drop(scene_pool);
            return Err(abort_candidate(candidate, error));
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
            drop(scene_pool);
            return Err(abort_candidate(candidate, error));
        }
    };
    let registration = match Registration::new(output) {
        Ok(registration) => registration,
        Err(error) => {
            drop(scene_pool);
            return Err(abort_candidate(candidate, error));
        }
    };
    let layout = registration.layout();
    let video = match Video::start(registration, config.pipewire, remote).await {
        Ok(video) => video,
        Err(error) => {
            let _ = started.send(Started::Failed);
            drop(scene_pool);
            return Err(abort_candidate(candidate, error));
        }
    };
    let (candidate, storage) = match profile.register(candidate) {
        Ok(registered) => registered,
        Err(failure) => {
            let (candidate, error) = failure.into_parts();
            drop(scene_pool);
            return Err(finish_unregistered(video, candidate, error).await);
        }
    };
    let profile_registration = candidate.registration();
    let probe = match PrivateProbe::prepare(device, candidate) {
        Ok(probe) => probe,
        Err(failure) => {
            let (candidate, error) = failure.into_parts();
            drop(scene_pool);
            return Err(finish_registered(video, candidate, error).await);
        }
    };
    let identity = video.identity().clone();
    if started
        .send(Started::Ready {
            identity,
            layout,
            registration: profile_registration,
        })
        .is_err()
    {
        let failure = finish_candidate(
            video,
            probe,
            scene_pool,
            Some(io::Error::other("renderer stream setup was abandoned")),
        )
        .await
        .expect_err("explicit renderer setup failure remains terminal");
        return Err(failure);
    }
    Ok(PreparedGeneration {
        video,
        probe,
        storage,
        scene_pool,
        source_interval: Duration::from_nanos(interval_ns),
        scene_images,
    })
}

fn abort_candidate<F: AsFd>(candidate: TakeoverCandidate<'_, F>, error: io::Error) -> io::Error {
    combine_abort(error, candidate.abort())
}

async fn finish_unregistered<F: AsFd>(
    video: Video,
    candidate: TakeoverCandidate<'_, F>,
    error: io::Error,
) -> io::Error {
    let error = abort_candidate(candidate, error);
    finish_video(video, Some(error))
        .await
        .expect_err("explicit setup failure remains terminal")
}

async fn finish_registered<F: AsFd>(
    video: Video,
    candidate: RegisteredCandidate<'_, F>,
    error: io::Error,
) -> io::Error {
    let error = combine_abort(error, candidate.abort());
    finish_video(video, Some(error))
        .await
        .expect_err("explicit setup failure remains terminal")
}

fn combine_abort(error: io::Error, abort: io::Result<()>) -> io::Error {
    let mut failure = Some(error);
    if let Err(error) = abort {
        add_failure(&mut failure, "abort renderer takeover", error);
    }
    failure.expect("setup failure initializes error accumulation")
}

fn add_failure(failure: &mut Option<io::Error>, operation: &str, error: io::Error) {
    *failure = Some(match failure.take() {
        Some(primary) => io::Error::new(primary.kind(), format!("{primary}; {operation}: {error}")),
        None => io::Error::new(error.kind(), format!("{operation}: {error}")),
    });
}

async fn finish_candidate<F: AsFd>(
    video: Video,
    probe: PrivateProbe<'_, F>,
    scene_pool: pronk_renderer_worker::ScenePool,
    mut failure: Option<io::Error>,
) -> io::Result<()> {
    if let Err(error) = probe.abort() {
        add_failure(&mut failure, "abort renderer takeover", error);
    }
    drop(scene_pool);
    finish_video(video, failure).await
}

async fn finish_video(video: Video, mut failure: Option<io::Error>) -> io::Result<()> {
    let stopped = video.shutdown().await;
    if let Err(error) = stopped.finish().await {
        add_failure(&mut failure, "stop renderer video", error);
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
    use super::{add_failure, combine_abort};
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

    #[test]
    fn cleanup_accumulates_every_failure_under_the_primary_error_class() {
        let mut failure = Some(io::Error::new(io::ErrorKind::BrokenPipe, "renderer failed"));
        add_failure(
            &mut failure,
            "abort renderer takeover",
            io::Error::from_raw_os_error(nix::libc::EBUSY),
        );
        add_failure(
            &mut failure,
            "stop renderer video",
            io::Error::other("PipeWire stopped"),
        );

        let failure = failure.unwrap();
        assert_eq!(failure.kind(), io::ErrorKind::BrokenPipe);
        assert!(failure.to_string().starts_with("renderer failed;"));
        assert!(failure.to_string().contains("abort renderer takeover:"));
        assert!(failure
            .to_string()
            .ends_with("stop renderer video: PipeWire stopped"));
    }
}
