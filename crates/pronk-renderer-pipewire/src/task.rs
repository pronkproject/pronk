//! Private orchestration for a renderer generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use castkms_renderer::Renderer;
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoBufferLayout, VideoNodeIdentity};
use pronk_renderer_worker::{
    CompletedReturn, OutputPool, PreparedSceneImages, PrimarySceneProfile, PrivateProbe,
    SceneReader,
};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::active;
use crate::registration::Registration;
use crate::types::{RendererStreamConfig, RendererStreamState};
use crate::video::Video;

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
}

pub(crate) async fn run<F: AsFd + Send + 'static>(
    renderer: Renderer<F>,
    device: Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    control: TaskControl,
) -> (Option<F>, io::Result<()>) {
    let state = control.state.clone();
    let result = run_generation(
        renderer,
        device,
        config,
        remote,
        GenerationControl {
            stop: &control.stop,
            started: control.started,
            state: &control.state,
        },
    )
    .await;
    state.send_replace(match &result {
        Ok(()) => RendererStreamState::Stopped,
        Err(error) => RendererStreamState::Failed(error.to_string()),
    });
    (None, result)
}

async fn run_generation<F: AsFd>(
    renderer: Renderer<F>,
    device: Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    control: GenerationControl<'_>,
) -> io::Result<()> {
    let generation =
        match prepare_generation(renderer, &device, config, remote, control.started).await {
            Ok(generation) => generation,
            Err(error) => return Err(error),
        };
    run_prepared(generation, control.stop, control.state).await
}

struct GenerationControl<'a> {
    stop: &'a CancellationToken,
    started: oneshot::Sender<Started>,
    state: &'a watch::Sender<RendererStreamState>,
}

async fn run_prepared<F: AsFd>(
    generation: PreparedGeneration<F>,
    stop: &CancellationToken,
    state: &watch::Sender<RendererStreamState>,
) -> io::Result<()> {
    let PreparedGeneration {
        mut video,
        reader,
        source_interval,
    } = generation;
    state.send_replace(RendererStreamState::Active);
    let result = active::run_complete_scenes(
        reader,
        &mut video,
        VecDeque::new(),
        JoinSet::new(),
        source_interval,
        stop,
    )
    .await;
    finish_video(video, JoinSet::new(), result.err()).await
}

struct PreparedGeneration<F: AsFd> {
    video: Video,
    reader: SceneReader<F>,
    source_interval: Duration,
}

async fn prepare_generation<F: AsFd>(
    renderer: Renderer<F>,
    device: &Device,
    config: RendererStreamConfig,
    remote: PipeWireRemote,
    started: oneshot::Sender<Started>,
) -> io::Result<PreparedGeneration<F>> {
    let source_interval = config.pipewire.frame_rate.frame_interval().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "renderer frame rate exceeds the source clock resolution",
        )
    })?;
    let width = config.output_width;
    let height = config.output_height;
    let output_extent = Extent::new(width.get(), height.get())
        .expect("nonzero renderer dimensions form a valid extent");
    let profile = match PrimarySceneProfile::discover(device, output_extent) {
        Ok(profile) => profile,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let scene_pool = match profile.create_pool(config.private_capacity, config.private_capacity) {
        Ok(scene_pool) => scene_pool,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let scene_images = match PreparedSceneImages::new(
        device,
        width,
        height,
        config.output_modifier,
        config.private_capacity,
    ) {
        Ok(images) => images,
        Err(error) => {
            let _ = started.send(Started::Failed);
            drop(scene_pool);
            return Err(error);
        }
    };
    let output = match OutputPool::new(
        device,
        width,
        height,
        config.output_modifier,
        config.output_capacity,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            drop(scene_pool);
            return Err(error);
        }
    };
    let registration = match Registration::new(output) {
        Ok(registration) => registration,
        Err(error) => {
            drop(scene_pool);
            return Err(error);
        }
    };
    let layout = registration.layout();
    let video = match Video::start(registration, config.pipewire, remote).await {
        Ok(video) => video,
        Err(error) => {
            let _ = started.send(Started::Failed);
            drop(scene_pool);
            return Err(error);
        }
    };
    let (constraints, storage) = profile.into_parts();
    let mut draft = match renderer.prepare(&constraints, output_extent) {
        Ok(draft) => draft,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            return Err(finish_setup(video, error).await);
        }
    };
    let scene_images = match scene_images.register(&mut draft) {
        Ok(images) => images,
        Err(error) => return Err(finish_setup(video, error).await),
    };
    let probe = match PrivateProbe::prepare(device, draft) {
        Ok(probe) => probe,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            return Err(finish_setup(video, error).await);
        }
    };
    let probed = match probe.submit() {
        Ok(probed) => probed,
        Err(failure) => return Err(finish_setup(video, failure.into_error()).await),
    };
    let published = match probed.publish() {
        Ok(published) => published,
        Err(failure) => return Err(finish_setup(video, failure.into_error()).await),
    };
    let reader = match SceneReader::new(published, storage, scene_pool, scene_images) {
        Ok(reader) => reader,
        Err(failure) => {
            let (_, _, _, error) = failure.into_parts();
            return Err(finish_setup(video, error).await);
        }
    };
    let identity = video.identity().clone();
    if started.send(Started::Ready { identity, layout }).is_err() {
        drop(reader);
        return Err(finish_setup(
            video,
            io::Error::other("renderer stream setup was abandoned"),
        )
        .await);
    }
    Ok(PreparedGeneration {
        video,
        reader,
        source_interval,
    })
}

async fn finish_setup(video: Video, error: io::Error) -> io::Error {
    finish_video(video, JoinSet::new(), Some(error))
        .await
        .expect_err("explicit setup failure remains terminal")
}

fn add_failure(failure: &mut Option<io::Error>, operation: &str, error: io::Error) {
    *failure = Some(match failure.take() {
        Some(primary) => io::Error::new(primary.kind(), format!("{primary}; {operation}: {error}")),
        None => io::Error::new(error.kind(), format!("{operation}: {error}")),
    });
}

async fn finish_video(
    mut video: Video,
    mut reader_waits: JoinSet<CompletedReturn>,
    mut failure: Option<io::Error>,
) -> io::Result<()> {
    while let Some(returned) = reader_waits.join_next().await {
        match returned {
            Ok(returned) => {
                if let Err(error) = video.finish_return(returned) {
                    add_failure(&mut failure, "finish output return", error);
                }
            }
            Err(error) => add_failure(
                &mut failure,
                "join output return wait",
                io::Error::other(error),
            ),
        }
    }
    let stopped = video.shutdown().await;
    if let Err(error) = stopped.finish().await {
        add_failure(&mut failure, "stop renderer video", error);
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::add_failure;
    use std::io;

    #[test]
    fn cleanup_accumulates_every_failure_under_the_primary_error_class() {
        let mut failure = Some(io::Error::new(io::ErrorKind::BrokenPipe, "renderer failed"));
        add_failure(
            &mut failure,
            "withdraw renderer offer",
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
        assert!(failure.to_string().contains("withdraw renderer offer:"));
        assert!(failure
            .to_string()
            .ends_with("stop renderer video: PipeWire stopped"));
    }
}
