//! Private orchestration for a renderer generation.

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;

use castkms_renderer::Renderer;
use drm_display_executor::scene::geometry::Extent;
use pronk_gpu::vulkan::Device;
use pronk_renderer_worker::{PreparedSceneImages, PrimarySceneProfile, SceneReader};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::activation::PreparedRendererConfiguration;
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
    let modifiers = device.private_storage_modifiers(
        config.output_format,
        config.output_width,
        config.output_height,
    )?;
    let modifier = choose_private_modifier(&modifiers, config.private_pool.modifier)?;
    let (frame_capacity, source_capacity) = profile.bounded_capacities(
        config.private_pool.frame_capacity,
        config.private_pool.source_capacity,
    )?;
    require_refresh_capacity(frame_capacity)?;
    let scene_pool = profile.create_pool(frame_capacity, source_capacity)?;
    let scene_images = match PreparedSceneImages::new(
        device,
        config.output_width,
        config.output_height,
        config.output_format,
        modifier,
        frame_capacity,
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
    let configuration = match PreparedRendererConfiguration::prepare(configuration, device) {
        Ok(preparation) => preparation,
        Err(failure) => {
            let (_, error) = failure.into_parts();
            return Err(error);
        }
    };
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

fn require_refresh_capacity(capacity: NonZeroUsize) -> io::Result<()> {
    if capacity.get() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "renderer needs two final images to refresh its retained frame",
        ));
    }
    Ok(())
}

fn choose_private_modifier(available: &[u64], requested: Option<u64>) -> io::Result<u64> {
    match requested {
        Some(modifier) if available.contains(&modifier) => Ok(modifier),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the selected GPU cannot allocate the requested private scene layout",
        )),
        None => available
            .iter()
            .copied()
            .find(|modifier| *modifier == 0)
            .or_else(|| available.first().copied())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the selected GPU has no exportable private scene layout",
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::{choose_private_modifier, require_refresh_capacity, Started};
    use drm_display_executor::scene::geometry::Extent;
    use std::io;
    use std::num::NonZeroUsize;

    #[test]
    fn retained_frame_requires_another_slot_for_refresh() {
        let one = NonZeroUsize::new(1).unwrap();
        let error = require_refresh_capacity(one).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("two final images"));
        assert!(require_refresh_capacity(NonZeroUsize::new(2).unwrap()).is_ok());
    }

    #[test]
    fn setup_result_carries_only_renderer_geometry() {
        let ready = Started::Ready {
            output: Extent::new(1920, 1080).unwrap(),
        };
        assert!(matches!(ready, Started::Ready { output } if output.width() == 1920));
    }

    #[test]
    fn private_storage_uses_a_checked_layout_without_requiring_linear() {
        assert_eq!(choose_private_modifier(&[1, 9], None).unwrap(), 1);
        assert_eq!(choose_private_modifier(&[0, 1, 9], None).unwrap(), 0);
        assert_eq!(choose_private_modifier(&[0, 9], Some(9)).unwrap(), 9);
        assert!(choose_private_modifier(&[0, 9], Some(1)).is_err());
        assert!(choose_private_modifier(&[], None).is_err());
    }
}
