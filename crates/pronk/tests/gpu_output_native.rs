//! Native reservation tests with synthetic actor events and completed fences.
//! No PipeWire server, renderer, encoder, or GPU job is exercised here.

use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use pronk::gpu_output::{GpuOutput, OutputEvent, OutputReady};
use pronk_dmabuf::{export_dependencies, Access};
use pronk_gpu::output_pool::OutputPool;
use pronk_pipewire::{
    PipeWireBufferTransport, VideoDamage, VideoFrame, VideoNodeIdentity, VideoSourceActorEvent,
    VideoSourceStopReport,
};

#[repr(C)]
struct Allocation {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}
nix::ioctl_readwrite!(allocate, b'H', 0, Allocation);

fn buffer() -> OwnedFd {
    let heap = std::fs::File::open("/dev/dma_heap/system").unwrap();
    let mut request = Allocation {
        len: 4096,
        fd: 0,
        fd_flags: (nix::libc::O_RDWR | nix::libc::O_CLOEXEC) as u32,
        heap_flags: 0,
    };
    // SAFETY: Writable UAPI request lives through the ioctl returning an owned fd.
    unsafe { allocate(heap.as_raw_fd(), &mut request) }.unwrap();
    let fd = i32::try_from(request.fd).unwrap();
    // SAFETY: Successful allocation returned a new owned descriptor.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn id() -> NonZeroU32 {
    NonZeroU32::new(7).unwrap()
}

fn identity() -> VideoNodeIdentity {
    VideoNodeIdentity {
        node_name: "gpu-test".into(),
        object_id: id(),
        object_serial: NonZeroU64::new(42).unwrap(),
        media_generation: NonZeroU64::new(3).unwrap(),
    }
}

async fn owner() -> GpuOutput {
    let mut owner = GpuOutput::new(
        identity(),
        OutputPool::new(vec![buffer()]).unwrap(),
        vec![id()],
    )
    .unwrap();
    let event = VideoSourceActorEvent::BufferAvailable {
        media_generation: identity().media_generation,
        buffer_id: id(),
        transport: PipeWireBufferTransport::ReadyBeforePublish,
    };
    let OutputEvent::Wait(wait) = owner.handle_event(&event).unwrap() else {
        panic!("initial wait")
    };
    assert!(owner.claim(id()).is_err());
    owner.complete(wait.wait().await).unwrap();
    owner
}

async fn publish(owner: &mut GpuOutput, sequence: u64) {
    let write = owner.claim(id()).unwrap();
    let fence =
        export_dependencies(owner.write_buffer(&write).unwrap(), Access::ReadWrite).unwrap();
    let wait = owner.submitted(write, fence).unwrap();
    let OutputReady::Publish(ready) = owner.complete(wait.wait().await).unwrap() else {
        panic!("producer completion")
    };
    owner
        .begin_publish(
            ready,
            VideoFrame {
                buffer_id: id(),
                sequence,
                pts_ns: 0,
                damage: VideoDamage {
                    x: 0,
                    y: 0,
                    width: id(),
                    height: id(),
                },
                discontinuity: false,
                acquire_point: None,
            },
        )
        .unwrap();
}

fn release(sequence: u64) -> VideoSourceActorEvent {
    VideoSourceActorEvent::BufferReleased {
        media_generation: identity().media_generation,
        buffer_id: id(),
        sequence,
    }
}

#[tokio::test]
#[ignore = "requires access to /dev/dma_heap/system"]
async fn stale_release_cannot_release_a_republished_destination() {
    let mut owner = owner().await;
    publish(&mut owner, 10).await;
    let OutputEvent::Wait(wait) = owner.handle_event(&release(10)).unwrap() else {
        panic!("reader wait")
    };
    assert!(owner.claim(id()).is_err());
    owner.complete(wait.wait().await).unwrap();
    publish(&mut owner, 11).await;
    assert!(owner.handle_event(&release(10)).is_err());
    assert!(owner.claim(id()).is_err());
    let mut stale = release(11);
    if let VideoSourceActorEvent::BufferReleased {
        media_generation, ..
    } = &mut stale
    {
        *media_generation = NonZeroU64::new(2).unwrap();
    }
    assert!(matches!(
        owner.handle_event(&stale).unwrap(),
        OutputEvent::Ignored
    ));
    let OutputEvent::Wait(wait) = owner.handle_event(&release(11)).unwrap() else {
        panic!("reader wait")
    };
    owner.complete(wait.wait().await).unwrap();
    assert!(owner.handle_event(&release(11)).is_err());
    assert!(owner.claim(id()).is_ok());
}

#[tokio::test]
#[ignore = "requires access to /dev/dma_heap/system"]
async fn shutdown_retires_unacknowledged_or_unconsumed_publications() {
    let mut owner = owner().await;
    // No successful handoff acknowledgement is supplied to the owner.
    publish(&mut owner, 1).await;
    let mut wrong = identity();
    wrong.object_serial = NonZeroU64::new(43).unwrap();
    assert!(owner
        .stopped(&VideoSourceStopReport {
            identity: wrong,
            reclaimed_buffers: Box::new([])
        })
        .is_err());
    // A release may already have left the actor, so its reclaim list is empty.
    let retired = owner
        .stopped(&VideoSourceStopReport {
            identity: identity(),
            reclaimed_buffers: Box::new([]),
        })
        .unwrap();
    assert!(retired.errors.is_empty());
    assert_eq!(retired.waits.len(), 1);
    for wait in retired.waits {
        owner.complete(wait.wait().await).unwrap();
    }
    assert!(owner.claim(id()).is_err());
    assert!(matches!(
        owner.handle_event(&release(1)).unwrap(),
        OutputEvent::Ignored
    ));
}
