use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use pronk_gpu::vulkan::Device;
use pronk_pipewire::{PipeWireRemote, VideoFrameRate, VideoSourceConfig};
use pronk_renderer_pipewire::{Registration, Video};
use pronk_renderer_worker::OutputPool;

#[tokio::test]
#[ignore = "requires explicit Vulkan GPU, modifier, and development PipeWire access"]
async fn video_generation_starts_and_quiesces_without_frames() {
    let node = std::env::var("PRONK_GPU_RENDER_NODE").expect("PRONK_GPU_RENDER_NODE");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("PRONK_GPU_MODIFIER");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let pool = OutputPool::new(
        &device,
        NonZeroU32::new(64).unwrap(),
        NonZeroU32::new(32).unwrap(),
        modifier,
        NonZeroUsize::new(2).unwrap(),
    )
    .await
    .unwrap();
    let registration = Registration::new(pool).unwrap();
    let generation = NonZeroU64::new(71).unwrap();
    let video = Video::start(
        registration,
        VideoSourceConfig {
            node_name: format!("pronk.renderer-test.{}", std::process::id()),
            node_description: "Pronk renderer output test".into(),
            session_id: "renderer-test".into(),
            device_instance: "renderer-test".into(),
            connector_id: NonZeroU32::new(1).unwrap(),
            output_index: 0,
            media_generation: generation,
            frame_rate: VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
        },
        PipeWireRemote::AmbientDevelopment,
    )
    .await
    .unwrap();
    assert_eq!(video.identity().media_generation, generation);

    let stopped = video.shutdown().await;
    assert!(stopped.error().is_none());
    stopped.finish().await.unwrap();
}
