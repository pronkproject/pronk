//! Select one capture capability and construct its application media pipeline.

use std::io;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::time::Duration;

use castkms_sys::DRM_FORMAT_MOD_LINEAR;
use pronk_pipewire::{ClassifiedSocketRemoteProvider, VideoFrameRate};

use crate::capture_health::CaptureEvents;
use crate::drm_capture_pipeline::{DrmCapturePipeline, DrmCapturePipelineConfig};
use crate::media_pipeline_port::CapturePipelinePort;
use crate::renderer_capture_pipeline::{RendererCapturePipeline, RendererCapturePipelineConfig};
use crate::renderer_session::RendererAccess;

/// Explicit source of media images, independent of the authorization issuer.
///
/// Final-image capture does not select the display's renderer. Neither choice
/// permits switching to the other after a capability or pipeline error.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    #[default]
    Renderer,
    FinalImage,
}

/// Authority for the selected media path, without the display lifetime.
#[derive(Debug)]
pub(crate) enum DisplayMediaAccess {
    Renderer(RendererAccess),
    FinalImage(drm_capture::Access),
}

pub(crate) struct DisplayMediaConfig {
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub session_id: String,
    pub device_instance: String,
    pub node_description: String,
    pub video_profile_id: String,
    pub video_bitrate: NonZeroU64,
    pub video_frame_rate: VideoFrameRate,
}

impl DisplayMediaAccess {
    pub(crate) async fn release(self) -> io::Result<()> {
        match self {
            Self::Renderer(renderer) => renderer.release().await,
            Self::FinalImage(capture) => {
                drop(capture);
                Ok(())
            }
        }
    }

    pub(crate) fn create_pipeline(
        self,
        remotes: ClassifiedSocketRemoteProvider,
        config: DisplayMediaConfig,
    ) -> io::Result<(Box<dyn CapturePipelinePort>, CaptureEvents)> {
        match self {
            Self::Renderer(renderer) => {
                let (pipeline, events) = RendererCapturePipeline::new(
                    renderer,
                    remotes,
                    RendererCapturePipelineConfig {
                        connector_id: config.connector_id,
                        output_index: config.output_index,
                        session_id: config.session_id,
                        device_instance: config.device_instance,
                        node_description: config.node_description,
                        video_profile_id: config.video_profile_id,
                        video_bitrate: config.video_bitrate,
                        video_frame_rate: config.video_frame_rate,
                        output_modifier: DRM_FORMAT_MOD_LINEAR,
                        private_capacity: NonZeroUsize::new(3).unwrap(),
                        output_capacity: NonZeroUsize::new(4).unwrap(),
                    },
                )?;
                Ok((Box::new(pipeline), events))
            }
            Self::FinalImage(capture) => {
                let (pipeline, events) = DrmCapturePipeline::new(
                    capture,
                    remotes,
                    DrmCapturePipelineConfig {
                        connector_id: config.connector_id,
                        output_index: config.output_index,
                        session_id: config.session_id,
                        device_instance: config.device_instance,
                        node_description: config.node_description,
                        video_profile_id: config.video_profile_id,
                        video_bitrate: config.video_bitrate,
                        video_frame_rate: config.video_frame_rate,
                        pool_size: NonZeroU32::new(4).unwrap(),
                        request_capacity: NonZeroU32::new(3).unwrap(),
                        pool_byte_limit: NonZeroU64::new(128 * 1024 * 1024).unwrap(),
                        heap_path: "/dev/dma_heap/system".into(),
                        poll_interval: Duration::from_millis(2),
                        shutdown_timeout: Duration::from_secs(5),
                    },
                );
                Ok((Box::new(pipeline), events))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_lease::CapabilityLease;
    use crate::display_state::{RouteTarget, RoutedMode};
    use crate::media_session::{MediaRoute, MediaStartRequest};
    use crate::renderer_session::{RendererProvider, RendererSession, RendererSessionError};
    use pronk_pipewire::ClassifiedSocketPaths;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn config() -> DisplayMediaConfig {
        DisplayMediaConfig {
            connector_id: NonZeroU32::new(5).unwrap(),
            output_index: 0,
            session_id: "session-test".into(),
            device_instance: "device-test".into(),
            node_description: "test output".into(),
            video_profile_id: "h264".into(),
            video_bitrate: NonZeroU64::new(4_000_000).unwrap(),
            video_frame_rate: VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
        }
    }

    fn remotes() -> ClassifiedSocketRemoteProvider {
        ClassifiedSocketRemoteProvider::new(
            ClassifiedSocketPaths::in_runtime_dir("/run/pronk-test/no-sockets").unwrap(),
        )
    }

    #[derive(Debug)]
    struct NoReplacement;

    #[async_trait::async_trait]
    impl RendererProvider for NoReplacement {
        async fn acquire(
            &self,
            _: CancellationToken,
        ) -> Result<RendererAccess, RendererSessionError> {
            panic!("pipeline construction does not obtain additional authority")
        }
    }

    #[tokio::test]
    async fn invalid_renderer_authority_does_not_select_final_image_capture() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let access = DisplayMediaAccess::Renderer(RendererAccess::new(
            std::fs::File::open("/dev/null").unwrap().into(),
            CapabilityLease::new(async move {
                let _ = send.send(());
                Ok(())
            }),
            "/dev/dri/renderD128".into(),
            RendererSession::new(Arc::new(NoReplacement), None),
        ));
        assert_eq!(
            access
                .create_pipeline(remotes(), config())
                .unwrap_err()
                .raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        tokio::time::timeout(Duration::from_secs(1), receive)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn final_image_setup_defers_io_until_an_uncancelled_start() {
        let access = DisplayMediaAccess::FinalImage(drm_capture::Access::from_fd(
            std::fs::File::open("/dev/null").unwrap().into(),
        ));
        let (mut pipeline, _events) = access.create_pipeline(remotes(), config()).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let request = MediaStartRequest {
            media_generation: 1,
            route: MediaRoute {
                route_generation: 1,
                target: RouteTarget::new(NonZeroU32::new(3).unwrap()),
                mode: RoutedMode {
                    width: 1280,
                    height: 720,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            },
        };
        let error = pipeline.start(request, cancellation).await.unwrap_err();
        assert_eq!(error.to_string(), "capture start was cancelled");
    }
}
