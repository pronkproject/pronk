use std::num::{NonZeroU32, NonZeroUsize};

use pronk_gpu::vulkan::Device;
use pronk_renderer_worker::{OutputPool, PrivatePool};

#[tokio::test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
async fn private_pixels_cross_output_ownership_without_source_state() {
    let node = std::env::var("PRONK_GPU_RENDER_NODE").expect("PRONK_GPU_RENDER_NODE");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("PRONK_GPU_MODIFIER");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let width = NonZeroU32::new(64).unwrap();
    let height = NonZeroU32::new(32).unwrap();
    let mut private =
        PrivatePool::new(&device, width, height, NonZeroUsize::new(1).unwrap()).unwrap();
    let mut output = OutputPool::new(
        &device,
        width,
        height,
        modifier,
        NonZeroUsize::new(2).unwrap(),
    )
    .await
    .unwrap();

    let source = private
        .take()
        .unwrap()
        .clear_and_wait([17, 34, 51])
        .unwrap();
    let completed = output.claim(0).unwrap().copy_from(source).unwrap();
    let pending = output.submit(completed).unwrap();
    let finished = pending.wait().await;
    let ready = output.finish(finished).unwrap();
    let (source, published) = output.publish(ready).unwrap();
    assert_eq!(published.content_serial(), None);
    assert!(private.put(source).is_ok());
    assert!(output.claim(0).is_err());

    let returned = output.begin_return(published).unwrap();
    assert_eq!(output.finish_return(returned.wait().await).unwrap(), 0);
    assert!(output.claim(0).is_ok());
}
