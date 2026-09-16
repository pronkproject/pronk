use std::num::{NonZeroU32, NonZeroUsize};

use pronk_gpu::vulkan::Device;
use pronk_pipewire::{VideoBufferStorage, VideoPixelFormat};
use pronk_renderer_pipewire::Registration;
use pronk_renderer_worker::OutputPool;

#[tokio::test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
async fn registration_exports_its_owned_renderer_pool() {
    let node = std::env::var("PRONK_GPU_RENDER_NODE").expect("PRONK_GPU_RENDER_NODE");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("PRONK_GPU_MODIFIER");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let width = NonZeroU32::new(64).unwrap();
    let height = NonZeroU32::new(32).unwrap();
    let capacity = NonZeroUsize::new(2).unwrap();
    let pool = OutputPool::new(&device, width, height, modifier, capacity)
        .await
        .unwrap();
    let layout = pool.layout();
    let registration = Registration::new(pool).unwrap();
    let buffers = registration.export().unwrap();
    assert_eq!(buffers.len(), 2);
    for (index, buffer) in buffers.iter().enumerate() {
        assert_eq!(buffer.id.get(), index as u32 + 1);
        assert_eq!(buffer.layout.format, VideoPixelFormat::Xrgb8888);
        assert_eq!(
            buffer.layout.storage,
            VideoBufferStorage::DrmModifier {
                modifier,
                offset: u32::try_from(layout.offset).unwrap(),
            }
        );
    }
}
