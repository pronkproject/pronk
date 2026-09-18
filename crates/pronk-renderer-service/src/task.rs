//! Private orchestration for a renderer generation.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::Renderer;
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;
use pronk_renderer_worker::{
    PreparedSceneImages, PrimarySceneProfile, PrivatePreparation, SceneReader,
};
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
    let (reader, output) = match prepare_generation(renderer, &device, config) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = control.started.send(Started::Failed);
            return Err(error);
        }
    };
    control.state.send_replace(RendererStreamState::Running);
    if control.started.send(Started::Ready { output }).is_err() {
        drop(reader);
        return Err(io::Error::other("renderer stream setup was abandoned"));
    }
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
) -> io::Result<(SceneReader<F>, Extent)> {
    let output = Extent::new(config.output_width.get(), config.output_height.get())
        .expect("nonzero renderer dimensions form a valid extent");
    let profile = PrimarySceneProfile::discover(device, output)?;
    let scene_pool = profile.create_pool(
        config.private_pool.frame_capacity,
        config.private_pool.source_capacity,
    )?;
    let scene_images = match PreparedSceneImages::new(
        device,
        config.output_width,
        config.output_height,
        config.private_pool.modifier,
        config.private_pool.frame_capacity,
    ) {
        Ok(images) => images,
        Err(error) => {
            drop(scene_pool);
            return Err(error);
        }
    };
    let (constraints, storage) = profile.into_parts();
    let mut configuration = match renderer.configure(&constraints, output) {
        Ok(configuration) => configuration,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            return Err(error);
        }
    };
    let scene_images = scene_images.register(&mut configuration)?;
    let preparation = match PrivatePreparation::prepare(device, configuration) {
        Ok(preparation) => preparation,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            return Err(error);
        }
    };
    let configuration = preparation.complete();
    let published = match configuration.publish(None) {
        Ok(published) => published,
        Err(failure) => return Err(failure.into_error()),
    };
    let reader = match SceneReader::new(published, storage, scene_pool, scene_images) {
        Ok(reader) => reader,
        Err(failure) => {
            let (_, _, _, error) = failure.into_parts();
            return Err(error);
        }
    };
    Ok((reader, output))
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
