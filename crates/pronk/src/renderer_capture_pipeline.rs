//! Userspace-rendered capture behind the application media port.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use async_trait::async_trait;
use pronk_capture_broker::RendererAccess;
use pronk_gpu::vulkan::Device;
use pronk_pipewire::{
    ClassifiedSocketRemoteProvider, VideoBufferLayout, VideoBufferStorage, VideoPixelFormat,
    VideoSourceConfig,
};
use pronk_renderer_pipewire::{
    ActiveRendererStream, RendererStream, RendererStreamConfig, RendererStreamError,
    RendererStreamState,
};
use tokio_util::sync::CancellationToken;

use crate::device_session_port::{DeviceMediaConfiguration, DeviceMediaKind, DeviceMediaTarget};
use crate::media_pipeline_port::{CapturePipelinePort, MediaPipelineError, PreparedCaptureMedia};
use crate::media_session::{MediaStartRequest, MediaStopReason, MediaSuspendReason};

/// Immutable GPU and media policy for one display's renderer pipeline.
#[derive(Debug, Clone)]
pub struct RendererCapturePipelineConfig {
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub session_id: String,
    pub device_instance: String,
    pub node_description: String,
    pub video_profile_id: String,
    pub video_bitrate: NonZeroU64,
    pub capture_rate_hz: NonZeroU32,
    pub render_node: PathBuf,
    pub output_modifier: u64,
    pub private_capacity: NonZeroUsize,
    pub output_capacity: NonZeroUsize,
}

enum Stream {
    Prepared(RendererStream<OwnedFd>),
    Active(ActiveRendererStream<OwnedFd>),
}

struct Generation {
    id: NonZeroU64,
    stream: Stream,
}

/// Sole owner of renderer authority and its per-generation GPU producer.
pub struct RendererCapturePipeline {
    renderer: RendererAccess,
    producer_remotes: ClassifiedSocketRemoteProvider,
    config: RendererCapturePipelineConfig,
    generation: Option<Generation>,
}

impl RendererCapturePipeline {
    pub fn new(
        renderer: RendererAccess,
        producer_remotes: ClassifiedSocketRemoteProvider,
        config: RendererCapturePipelineConfig,
    ) -> Self {
        Self {
            renderer,
            producer_remotes,
            config,
            generation: None,
        }
    }

    async fn stop_generation(&mut self, id: NonZeroU64) -> Result<(), MediaPipelineError> {
        let Some(generation) = self.generation.take() else {
            return Ok(());
        };
        if generation.id != id {
            let actual = generation.id;
            self.generation = Some(generation);
            return Err(MediaPipelineError::new(format!(
                "renderer stop requested generation {id}; active generation is {actual}"
            )));
        }
        match generation.stream {
            Stream::Prepared(stream) => {
                let owner = stream
                    .shutdown()
                    .await
                    .map_err(|error| stream_error("stop prepared renderer", error))?;
                drop(owner);
            }
            Stream::Active(stream) => stream
                .shutdown()
                .await
                .map_err(|error| stream_error("stop active renderer", error))?,
        }
        Ok(())
    }

    fn target(
        &self,
        stream: &RendererStream<OwnedFd>,
        generation: NonZeroU64,
    ) -> Result<DeviceMediaTarget, MediaPipelineError> {
        Ok(DeviceMediaTarget {
            kind: DeviceMediaKind::Video,
            node_name: stream.identity().node_name.clone(),
            object_serial: stream.identity().object_serial,
            session_id: self.config.session_id.clone(),
            device_instance: self.config.device_instance.clone(),
            connector_id: self.config.connector_id,
            output_index: self.config.output_index,
            media_generation: generation,
            caps: renderer_caps(stream.layout(), self.config.capture_rate_hz)?,
        })
    }
}

impl std::fmt::Debug for RendererCapturePipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RendererCapturePipeline")
            .field("config", &self.config)
            .field(
                "generation",
                &self.generation.as_ref().map(|generation| generation.id),
            )
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CapturePipelinePort for RendererCapturePipeline {
    async fn start(
        &mut self,
        request: MediaStartRequest,
        cancellation: CancellationToken,
    ) -> Result<PreparedCaptureMedia, MediaPipelineError> {
        if self.generation.is_some() {
            return Err(MediaPipelineError::new(
                "a previous renderer generation still requires cleanup",
            ));
        }
        if cancellation.is_cancelled() {
            return Err(MediaPipelineError::new("renderer start was cancelled"));
        }
        let generation = NonZeroU64::new(request.media_generation)
            .ok_or_else(|| MediaPipelineError::new("media generation must be nonzero"))?;
        let renderer = self
            .renderer
            .open()
            .map_err(|error| MediaPipelineError::new(format!("open renderer session: {error}")))?;
        let render_node = self.config.render_node.clone();
        let device_task = tokio::task::spawn_blocking(move || Device::open(render_node));
        let device = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(MediaPipelineError::new("renderer start was cancelled"));
            }
            result = device_task => result
                .map_err(|error| MediaPipelineError::new(format!("join renderer GPU setup: {error}")))?
                .map_err(|error| MediaPipelineError::new(format!("open renderer GPU: {error}")))?,
        };
        let remote = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(MediaPipelineError::new("renderer start was cancelled"));
            }
            result = self.producer_remotes.create_producer_remote() => result.map_err(|error| {
                MediaPipelineError::new(format!("connect renderer PipeWire producer: {error}"))
            })?,
        };
        let preparation = RendererStream::prepare(
            renderer,
            device,
            RendererStreamConfig {
                pipewire: VideoSourceConfig {
                    node_name: format!("pronk.video.{}.{generation}", self.config.session_id),
                    node_description: self.config.node_description.clone(),
                    session_id: self.config.session_id.clone(),
                    device_instance: self.config.device_instance.clone(),
                    connector_id: self.config.connector_id,
                    output_index: self.config.output_index,
                    media_generation: generation,
                    refresh_hz: self.config.capture_rate_hz,
                },
                output_modifier: self.config.output_modifier,
                private_capacity: self.config.private_capacity,
                output_capacity: self.config.output_capacity,
            },
            remote.into_remote(),
        );
        tokio::pin!(preparation);
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(MediaPipelineError::new("renderer start was cancelled"));
            }
            result = &mut preparation => result
                .map_err(|error| stream_error("prepare renderer stream", error))?,
        };
        if cancellation.is_cancelled() {
            let owner = stream
                .shutdown()
                .await
                .map_err(|error| stream_error("cancel renderer stream", error))?;
            drop(owner);
            return Err(MediaPipelineError::new("renderer start was cancelled"));
        }
        let layout = stream.layout();
        if layout.width.get() != request.route.mode.width
            || layout.height.get() != request.route.mode.height
        {
            let actual = (layout.width, layout.height);
            let owner = stream
                .shutdown()
                .await
                .map_err(|error| stream_error("stop mismatched renderer stream", error))?;
            drop(owner);
            return Err(MediaPipelineError::new(format!(
                "renderer output is {}x{}; active route is {}x{}",
                actual.0, actual.1, request.route.mode.width, request.route.mode.height
            )));
        }
        let target = self.target(&stream, generation)?;
        self.generation = Some(Generation {
            id: generation,
            stream: Stream::Prepared(stream),
        });
        Ok(PreparedCaptureMedia {
            media_generation: generation,
            video_target: target,
            audio_target: None,
            configuration: DeviceMediaConfiguration {
                video_profile_id: self.config.video_profile_id.clone(),
                audio_profile_id: None,
                mode: request.route.mode,
                video_bitrate: self.config.video_bitrate,
            },
        })
    }

    async fn activate(
        &mut self,
        media_generation: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        let Some(generation) = self.generation.take() else {
            return Err(MediaPipelineError::new(
                "no renderer generation is prepared",
            ));
        };
        if generation.id != media_generation {
            let actual = generation.id;
            self.generation = Some(generation);
            return Err(MediaPipelineError::new(format!(
                "renderer activation requested generation {media_generation}; prepared generation is {actual}"
            )));
        }
        match generation.stream {
            Stream::Active(stream) => {
                match stream.state() {
                    RendererStreamState::Active => {}
                    RendererStreamState::Failed(error) => {
                        let result = stream.shutdown().await;
                        return Err(result.err().map_or_else(
                            || MediaPipelineError::new(error),
                            |error| stream_error("join failed renderer stream", error),
                        ));
                    }
                    state => {
                        let _ = stream.shutdown().await;
                        return Err(MediaPipelineError::new(format!(
                            "renderer generation has invalid active state {state:?}"
                        )));
                    }
                }
                self.generation = Some(Generation {
                    id: generation.id,
                    stream: Stream::Active(stream),
                });
                Ok(())
            }
            Stream::Prepared(stream) => {
                let activation = stream.activate();
                tokio::pin!(activation);
                let stream = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return Err(MediaPipelineError::new("renderer activation was cancelled"));
                    }
                    result = &mut activation => result
                        .map_err(|error| stream_error("activate renderer stream", error))?,
                };
                self.generation = Some(Generation {
                    id: generation.id,
                    stream: Stream::Active(stream),
                });
                Ok(())
            }
        }
    }

    async fn suspend(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaSuspendReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.stop_generation(media_generation).await
    }

    async fn stop(
        &mut self,
        media_generation: NonZeroU64,
        _reason: MediaStopReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        self.stop_generation(media_generation).await
    }

    async fn shutdown(
        &mut self,
        _reason: MediaStopReason,
        _cancellation: CancellationToken,
    ) -> Result<(), MediaPipelineError> {
        if let Some(generation) = self.generation.as_ref().map(|generation| generation.id) {
            self.stop_generation(generation).await?;
        }
        Ok(())
    }
}

fn renderer_caps(
    layout: VideoBufferLayout,
    rate: NonZeroU32,
) -> Result<String, MediaPipelineError> {
    let fourcc = match layout.format {
        VideoPixelFormat::Xrgb8888 => "XR24",
        VideoPixelFormat::Argb8888 => "AR24",
    };
    let VideoBufferStorage::DrmModifier { modifier, .. } = layout.storage else {
        return Err(MediaPipelineError::new(
            "renderer output does not have a DRM modifier",
        ));
    };
    let drm_format = if modifier == 0 {
        fourcc.to_string()
    } else {
        format!("{fourcc}:0x{modifier:016x}")
    };
    Ok(format!(
        "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm_format},width={},height={},framerate={rate}/1",
        layout.width, layout.height
    ))
}

fn stream_error<F>(operation: &str, error: RendererStreamError<F>) -> MediaPipelineError {
    let (_, error) = error.into_parts();
    MediaPipelineError::new(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_caps_describe_the_registered_gpu_layout() {
        let caps = renderer_caps(
            VideoBufferLayout {
                format: VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(1920).unwrap(),
                height: NonZeroU32::new(1080).unwrap(),
                pitch: NonZeroU32::new(7680).unwrap(),
                size: NonZeroU64::new(8_294_400).unwrap(),
                storage: VideoBufferStorage::DrmModifier {
                    modifier: 0x100000000000001,
                    offset: 0,
                },
            },
            NonZeroU32::new(30).unwrap(),
        )
        .unwrap();
        assert_eq!(
            caps,
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=XR24:0x0100000000000001,width=1920,height=1080,framerate=30/1"
        );
    }

    #[test]
    fn linear_modifier_caps_use_the_plain_drm_format() {
        let caps = renderer_caps(
            VideoBufferLayout {
                format: VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(64).unwrap(),
                height: NonZeroU32::new(32).unwrap(),
                pitch: NonZeroU32::new(256).unwrap(),
                size: NonZeroU64::new(8192).unwrap(),
                storage: VideoBufferStorage::DrmModifier {
                    modifier: 0,
                    offset: 0,
                },
            },
            NonZeroU32::new(60).unwrap(),
        )
        .unwrap();
        assert!(caps.contains("drm-format=XR24,"));
        assert!(caps.ends_with("framerate=60/1"));
    }
}
