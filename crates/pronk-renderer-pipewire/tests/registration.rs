use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use pronk_gpu::vulkan::Device;
use pronk_pipewire::{
    PipeWireBufferTransport, VideoBufferStorage, VideoNodeIdentity, VideoPixelFormat,
    VideoSourceActorEvent, VideoSourceStopReport,
};
use pronk_renderer_pipewire::{OutputEvent, Registration};
use pronk_renderer_worker::{OutputPool, PrivatePool};

#[tokio::test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
async fn registration_exports_only_its_renderer_pool() {
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
    let other = OutputPool::new(&device, width, height, modifier, capacity)
        .await
        .unwrap();

    let registration = Registration::new(&pool).unwrap();
    let buffers = registration.export(&pool).unwrap();
    assert_eq!(buffers.len(), 2);
    assert!(registration.export(&other).is_err());
    for (index, buffer) in buffers.iter().enumerate() {
        assert_eq!(buffer.id.get(), index as u32 + 1);
        assert_eq!(buffer.layout.format, VideoPixelFormat::Xrgb8888);
        assert_eq!(
            buffer.layout.storage,
            VideoBufferStorage::DrmModifier {
                modifier,
                offset: u32::try_from(pool.layout().offset).unwrap(),
            }
        );
    }
}

#[tokio::test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
async fn publication_survives_until_the_matching_release() {
    let node = std::env::var("PRONK_GPU_RENDER_NODE").expect("PRONK_GPU_RENDER_NODE");
    let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("PRONK_GPU_MODIFIER");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    let device = Device::open(node).unwrap();
    let width = NonZeroU32::new(64).unwrap();
    let height = NonZeroU32::new(32).unwrap();
    let mut private =
        PrivatePool::new(&device, width, height, NonZeroUsize::new(1).unwrap()).unwrap();
    let mut pool = OutputPool::new(
        &device,
        width,
        height,
        modifier,
        NonZeroUsize::new(2).unwrap(),
    )
    .await
    .unwrap();
    let registration = Registration::new(&pool).unwrap();
    let identity = VideoNodeIdentity {
        node_name: "pronk-renderer-test".into(),
        object_id: NonZeroU32::new(7).unwrap(),
        object_serial: NonZeroU64::new(8).unwrap(),
        media_generation: NonZeroU64::new(9).unwrap(),
    };
    let mut transport = registration.bind(identity.clone());
    for buffer in 1..=2 {
        assert!(matches!(
            transport
                .handle_event(&VideoSourceActorEvent::BufferAvailable {
                    media_generation: identity.media_generation,
                    buffer_id: NonZeroU32::new(buffer).unwrap(),
                    transport: PipeWireBufferTransport::Waited,
                })
                .unwrap(),
            OutputEvent::Available { slot } if slot == (buffer - 1) as usize
        ));
    }

    let source = private.take().unwrap().clear_waited([17, 34, 51]).unwrap();
    let completed = pool.claim(0).unwrap().copy_from(source).unwrap();
    let pending = pool.submit(completed).unwrap();
    let ready = pool.finish(pending.wait().await).unwrap();
    let (source, published) = pool.publish(ready).unwrap();
    assert!(private.put(source).is_ok());
    let frame = transport
        .begin_publish(published, 123, true)
        .unwrap_or_else(|error| panic!("publish: {}", error.error()));
    assert_eq!(frame.buffer_id.get(), 1);
    assert_eq!(frame.sequence, 1);
    assert_eq!(frame.pts_ns, 123);
    assert!(frame.discontinuity);
    assert!(transport
        .handle_event(&VideoSourceActorEvent::BufferReleased {
            media_generation: identity.media_generation,
            buffer_id: frame.buffer_id,
            sequence: frame.sequence + 1,
        })
        .is_err());
    let published = match transport
        .handle_event(&VideoSourceActorEvent::BufferReleased {
            media_generation: identity.media_generation,
            buffer_id: frame.buffer_id,
            sequence: frame.sequence,
        })
        .unwrap()
    {
        OutputEvent::Released(output) => output,
        _ => panic!("matching release did not return publication ownership"),
    };
    let returned = pool.begin_return(published).unwrap();
    assert_eq!(pool.finish_return(returned.wait().await).unwrap(), 0);

    let source = private.take().unwrap().clear_waited([68, 85, 102]).unwrap();
    let completed = pool.claim(1).unwrap().copy_from(source).unwrap();
    let pending = pool.submit(completed).unwrap();
    let ready = pool.finish(pending.wait().await).unwrap();
    let (source, published) = pool.publish(ready).unwrap();
    assert!(private.put(source).is_ok());
    let frame = transport
        .begin_publish(published, 456, false)
        .unwrap_or_else(|error| panic!("publish: {}", error.error()));
    assert_eq!(frame.buffer_id.get(), 2);

    let reclaimed = match transport
        .stopped(&VideoSourceStopReport {
            identity,
            // The actor may know that a failed command never crossed its
            // handoff boundary even though the adapter retained ownership.
            reclaimed_buffers: Box::new([]),
        })
        .unwrap()
    {
        OutputEvent::Reclaimed(outputs) => outputs,
        _ => panic!("stop did not reclaim renderer publications"),
    };
    assert_eq!(reclaimed.len(), 1);
    let returned = pool
        .begin_return(reclaimed.into_vec().pop().unwrap())
        .unwrap();
    assert_eq!(pool.finish_return(returned.wait().await).unwrap(), 1);
    assert!(matches!(
        transport
            .handle_event(&VideoSourceActorEvent::BufferReleased {
                media_generation: NonZeroU64::new(9).unwrap(),
                buffer_id: frame.buffer_id,
                sequence: frame.sequence,
            })
            .unwrap(),
        OutputEvent::Ignored
    ));
}
