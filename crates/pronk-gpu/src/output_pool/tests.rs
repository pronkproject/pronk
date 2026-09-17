use super::*;
use std::os::unix::net::UnixStream;

fn pool(count: usize) -> OutputPool {
    OutputPool::new(
        (0..count)
            .map(|_| {
                let (socket, _peer) = UnixStream::pair().unwrap();
                socket.into()
            })
            .collect(),
    )
    .unwrap()
}

fn key(pool: &OutputPool, slot: usize) -> Key {
    Key {
        pool: pool.identity.clone(),
        slot,
        serial: pool.slots[slot].serial,
    }
}

// Inject native results to test ownership independently of a GPU driver.
fn finish(
    pool: &mut OutputPool,
    slot: usize,
    kind: WaitKind,
    result: io::Result<Completion>,
) -> io::Result<AccessReady> {
    pool.complete(FinishedAccess {
        key: key(pool, slot),
        kind,
        result,
    })
}

fn writable(pool: &mut OutputPool, slot: usize) {
    pool.slots[slot].state = State::WaitingForReaders;
    assert!(
        matches!(finish(pool, slot, WaitKind::Readers, Ok(Completion::Success)).unwrap(),
        AccessReady::Writable { slot: actual } if actual == slot)
    );
}

fn produced(pool: &mut OutputPool, permit: WritePermit) -> PublishPermit {
    pool.check(&permit.0, State::Writing).unwrap();
    pool.slots[permit.0.slot].state = State::WaitingForProducer;
    match finish(
        pool,
        permit.0.slot,
        WaitKind::Producer,
        Ok(Completion::Success),
    )
    .unwrap()
    {
        AccessReady::Publish(permit) => permit,
        _ => panic!("producer result must authorize publication"),
    }
}

#[test]
fn pool_bounds_and_allocation_aliases_are_checked() {
    assert!(OutputPool::new(vec![]).is_err());
    let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
    let alias = fd.try_clone().unwrap();
    assert!(OutputPool::new(vec![fd, alias]).is_err());
    let buffers = (0..=MAX_OUTPUT_BUFFERS)
        .map(|_| std::fs::File::open("/dev/null").unwrap().into())
        .collect();
    assert!(OutputPool::new(buffers).is_err());
    assert_eq!(pool(MAX_OUTPUT_BUFFERS).slots.len(), MAX_OUTPUT_BUFFERS);
}

#[test]
fn invalid_native_snapshot_quarantines_the_slot() {
    let mut pool = pool(1);
    assert!(pool.claim(0).is_err());
    assert!(pool.prepare_initial(0).is_err());
    assert_eq!(pool.state(0), Some(State::Quarantined));
    assert!(pool.prepare_initial(0).is_err());
    assert!(pool.claim(0).is_err());
}

#[test]
fn publication_and_reader_completion_are_distinct_boundaries() {
    let mut pool = pool(2);
    writable(&mut pool, 0);
    writable(&mut pool, 1);
    let write = pool.claim(0).unwrap();
    assert!(pool.write_buffer(&write).is_ok());
    assert!(pool.claim(0).is_err());
    let ready = produced(&mut pool, write);
    assert!(pool.claim(0).is_err());
    let publication = pool.publish(ready).unwrap();
    assert_eq!(publication.slot(), 0);
    assert_eq!(pool.state(0), Some(State::Published));
    assert!(pool.claim(0).is_err());
    // Returning transport ownership has begun a reader wait, not a new write.
    pool.slots[0].state = State::WaitingForReaders;
    assert!(pool.claim(0).is_err());
    let independent = pool.claim(1).unwrap();
    assert!(pool.write_buffer(&independent).is_ok());
    assert!(matches!(
        finish(&mut pool, 0, WaitKind::Readers, Ok(Completion::Success)).unwrap(),
        AccessReady::Writable { slot: 0 }
    ));
    assert!(pool.claim(0).is_ok());
}

#[test]
fn late_or_cross_pool_results_do_not_mutate_state() {
    let mut first = pool(1);
    let mut second = pool(1);
    first.slots[0].state = State::WaitingForReaders;
    second.slots[0].state = State::WaitingForReaders;
    let foreign = FinishedAccess {
        key: key(&first, 0),
        kind: WaitKind::Readers,
        result: Ok(Completion::Success),
    };
    assert!(second.complete(foreign).is_err());
    assert_eq!(second.state(0), Some(State::WaitingForReaders));
    let stale = FinishedAccess {
        key: key(&first, 0),
        kind: WaitKind::Readers,
        result: Ok(Completion::Success),
    };
    writable(&mut first, 0);
    let _write = first.claim(0).unwrap();
    first.slots[0].state = State::WaitingForReaders;
    assert!(first.complete(stale).is_err());
    assert_eq!(first.state(0), Some(State::WaitingForReaders));
    assert!(finish(&mut first, 0, WaitKind::Producer, Ok(Completion::Success)).is_err());
    assert_eq!(first.state(0), Some(State::WaitingForReaders));
}

#[test]
fn failed_native_access_is_never_publication_or_reuse() {
    for kind in [WaitKind::Readers, WaitKind::Producer] {
        for result in [
            Ok(Completion::Failed(-5)),
            Err(io::Error::from_raw_os_error(5)),
        ] {
            let mut pool = pool(1);
            pool.slots[0].state = match kind {
                WaitKind::Readers => State::WaitingForReaders,
                WaitKind::Producer => State::WaitingForProducer,
            };
            assert!(finish(&mut pool, 0, kind, result).is_err());
            assert_eq!(pool.state(0), Some(State::Quarantined));
            assert!(pool.claim(0).is_err());
        }
    }
}

#[test]
fn abandoned_permissions_do_not_recycle_storage() {
    let mut pool = pool(1);
    writable(&mut pool, 0);
    drop(pool.claim(0).unwrap());
    assert_eq!(pool.state(0), Some(State::Writing));
    assert!(pool.claim(0).is_err());
    let permit = WritePermit(key(&pool, 0));
    let ready = produced(&mut pool, permit);
    drop(ready);
    assert_eq!(pool.state(0), Some(State::ReadyToPublish));
    assert!(pool.claim(0).is_err());
}

#[test]
fn unpublished_completion_can_be_reused_without_transport_handoff() {
    let mut pool = pool(1);
    writable(&mut pool, 0);
    let write = pool.claim(0).unwrap();
    let ready = produced(&mut pool, write);

    assert_eq!(pool.discard(ready).unwrap(), 0);
    assert_eq!(pool.state(0), Some(State::Writable));
    assert!(pool.claim(0).is_ok());
}

#[test]
fn serial_exhaustion_never_reuses_an_old_identity() {
    let mut pool = pool(1);
    writable(&mut pool, 0);
    pool.slots[0].serial = u64::MAX;
    assert!(pool.claim(0).is_err());
    assert_eq!(pool.state(0), Some(State::Quarantined));
}

#[test]
fn invalid_slot_and_foreign_write_permit_are_rejected() {
    let mut first = pool(1);
    let mut second = pool(1);
    assert!(first.prepare_initial(1).is_err());
    assert!(first.claim(1).is_err());
    assert!(first.export(1).is_err());
    assert_eq!(first.state(1), None);
    writable(&mut first, 0);
    writable(&mut second, 0);
    let permit = first.claim(0).unwrap();
    assert!(second.write_buffer(&permit).is_err());
    assert_eq!(second.state(0), Some(State::Writable));
}
