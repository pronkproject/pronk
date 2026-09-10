//! Actual reservation ioctls with no-op completed fences, not GPU rendering.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use pronk_dmabuf::{export_dependencies, Access};
use pronk_gpu::output_pool::{AccessReady, OutputPool, State};

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
    // SAFETY: The writable Linux allocation request lives through the ioctl.
    unsafe { allocate(heap.as_raw_fd(), &mut request) }.unwrap();
    let fd = i32::try_from(request.fd).unwrap();
    // SAFETY: Successful allocation returns a new owned descriptor.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

#[tokio::test]
#[ignore = "requires access to /dev/dma_heap/system"]
async fn retained_destination_does_not_stop_other_slots() {
    let mut pool = OutputPool::new(vec![buffer(), buffer(), buffer()]).unwrap();
    for slot in 0..3 {
        let pending = pool.prepare_initial(slot).unwrap();
        assert!(pool.claim(slot).is_err());
        assert!(matches!(pool.complete(pending.wait().await).unwrap(),
            AccessReady::Writable { slot: ready } if ready == slot));
    }
    let mut held = None;
    for round in 0..4 {
        for slot in 0..3 {
            if slot == 0 && round != 0 {
                continue;
            }
            let permit = pool.claim(slot).unwrap();
            // An empty reservation supplies a completed no-op fence; no pixel
            // production or actual consumer GPU work is asserted by this test.
            let fence = export_dependencies(pool.write_buffer(&permit).unwrap(), Access::ReadWrite)
                .unwrap();
            let producer = pool.submitted(permit, fence).unwrap();
            assert_eq!(pool.state(slot), Some(State::WaitingForProducer));
            let ready = match pool.complete(producer.wait().await).unwrap() {
                AccessReady::Publish(ready) => ready,
                _ => panic!("producer wait must allow publication"),
            };
            let publication = pool.publish(ready).unwrap();
            if slot == 0 {
                held = Some(publication);
            } else {
                let readers = pool.returned(publication).unwrap();
                assert_eq!(pool.state(slot), Some(State::WaitingForReaders));
                assert!(pool.claim(slot).is_err());
                pool.complete(readers.wait().await).unwrap();
            }
        }
        assert_eq!(pool.state(0), Some(State::Published));
        assert!(pool.claim(0).is_err());
    }
    let returned = pool.returned(held.unwrap()).unwrap();
    pool.complete(returned.wait().await).unwrap();
    assert!(pool.claim(0).is_ok());
}

#[tokio::test]
#[ignore = "requires access to /dev/dma_heap/system"]
async fn cancelled_native_wait_never_releases_a_slot() {
    let mut pool = OutputPool::new(vec![buffer()]).unwrap();
    let pending = pool.prepare_initial(0).unwrap();
    let waiter = pending.wait();
    drop(waiter);
    assert_eq!(pool.state(0), Some(State::WaitingForReaders));
    assert!(pool.claim(0).is_err());
    assert!(pool.prepare_initial(0).is_err());
    // A transport registration descriptor remains an allocation reference.
    let exported = pool.export(0).unwrap();
    drop(pool);
    export_dependencies(exported.as_fd(), Access::ReadWrite)
        .unwrap()
        .wait()
        .await
        .unwrap();
}
