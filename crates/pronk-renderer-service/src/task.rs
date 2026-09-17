//! Private orchestration for a renderer generation.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::Renderer;
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;
use pronk_renderer_worker::{PreparedSceneImages, PrimarySceneProfile, PrivateProbe, SceneReader};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::active;
use crate::types::{RendererStreamConfig, RendererStreamState};

pub(crate) enum Started {
    Ready { output: Extent },
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
    control: TaskControl,
) -> (Option<F>, io::Result<()>) {
    let state = control.state.clone();
    let result = run_generation(
        renderer,
        device,
        config,
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
    control: GenerationControl<'_>,
) -> io::Result<()> {
    let reader = prepare_generation(renderer, &device, config, control.started)?;
    control.state.send_replace(RendererStreamState::Active);
    active::run_complete_scenes(reader, config.source_interval, control.stop).await
}

struct GenerationControl<'a> {
    stop: &'a CancellationToken,
    started: oneshot::Sender<Started>,
    state: &'a watch::Sender<RendererStreamState>,
}

fn prepare_generation<F: AsFd>(
    renderer: Renderer<F>,
    device: &Device,
    config: RendererStreamConfig,
    started: oneshot::Sender<Started>,
) -> io::Result<SceneReader<F>> {
    let output = Extent::new(config.output_width.get(), config.output_height.get())
        .expect("nonzero renderer dimensions form a valid extent");
    let profile = match PrimarySceneProfile::discover(device, output) {
        Ok(profile) => profile,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let scene_pool = match profile.create_pool(
        config.private_pool.frame_capacity,
        config.private_pool.source_capacity,
    ) {
        Ok(scene_pool) => scene_pool,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let scene_images = match PreparedSceneImages::new(
        device,
        config.output_width,
        config.output_height,
        config.private_pool.modifier,
        config.private_pool.frame_capacity,
    ) {
        Ok(images) => images,
        Err(error) => {
            let _ = started.send(Started::Failed);
            drop(scene_pool);
            return Err(error);
        }
    };
    let (constraints, storage) = profile.into_parts();
    let mut draft = match renderer.prepare(&constraints, output) {
        Ok(draft) => draft,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let scene_images = match scene_images.register(&mut draft) {
        Ok(images) => images,
        Err(error) => {
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let probe = match PrivateProbe::prepare(device, draft) {
        Ok(probe) => probe,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    let probed = match probe.submit() {
        Ok(probed) => probed,
        Err(failure) => {
            let _ = started.send(Started::Failed);
            return Err(failure.into_error());
        }
    };
    let published = match probed.publish() {
        Ok(published) => published,
        Err(failure) => {
            let _ = started.send(Started::Failed);
            return Err(failure.into_error());
        }
    };
    let reader = match SceneReader::new(published, storage, scene_pool, scene_images) {
        Ok(reader) => reader,
        Err(failure) => {
            let (_, _, _, error) = failure.into_parts();
            let _ = started.send(Started::Failed);
            return Err(error);
        }
    };
    if started.send(Started::Ready { output }).is_err() {
        drop(reader);
        return Err(io::Error::other("renderer stream setup was abandoned"));
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::Started;
    use drm_display_executor::scene::geometry::Extent;

    #[test]
    fn setup_result_carries_only_renderer_geometry() {
        let ready = Started::Ready {
            output: Extent::new(1920, 1080).unwrap(),
        };
        assert!(matches!(ready, Started::Ready { output } if output.width() == 1920));
    }
}
