use std::num::{NonZeroU32, NonZeroUsize};

use pronk_gpu::vulkan::Device;
use pronk_renderer_worker::PrivatePool;

#[test]
#[ignore = "requires PRONK_GPU_RENDER_NODE"]
fn private_pool_is_bounded_and_rejects_foreign_buffers() {
    let node = std::env::var("PRONK_GPU_RENDER_NODE").expect("PRONK_GPU_RENDER_NODE");
    let device = Device::open(node).unwrap();
    let width = NonZeroU32::new(64).unwrap();
    let height = NonZeroU32::new(32).unwrap();
    let mut first =
        PrivatePool::new(&device, width, height, NonZeroUsize::new(2).unwrap()).unwrap();
    let mut second =
        PrivatePool::new(&device, width, height, NonZeroUsize::new(1).unwrap()).unwrap();

    let a = first.take().unwrap();
    let b = first.take().unwrap();
    assert!(first.take().is_none());
    assert_eq!(first.available(), 0);
    assert!(first.put(a).is_ok());
    assert_eq!(first.available(), 1);

    let foreign = match second.put(b) {
        Ok(()) => panic!("foreign pool accepted a buffer"),
        Err(error) => error.into_buffer(),
    };
    assert!(first.put(foreign).is_ok());
    assert_eq!(first.available(), first.capacity().get());
}
