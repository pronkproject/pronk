//! Destination ownership for the private-image to exported-image copy.
//!
//! No compositor source or source-read lease belongs in this pool. Its waits
//! must precede admitting a source-reading job, or follow a copy into independent
//! private storage. Each pool has one immutable recipient authorization scope.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use pronk_dmabuf::{export_dependencies, import_completion, Access, Completion, SyncFile};

/// A destination budget, independent of source, encoder, or network budgets.
pub const MAX_OUTPUT_BUFFERS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unprepared,
    WaitingForReaders,
    Writable,
    Writing,
    WaitingForProducer,
    ReadyToPublish,
    Published,
    Quarantined,
}

#[derive(Debug)]
struct Slot {
    buffer: OwnedFd,
    state: State,
    serial: u64,
}

#[derive(Debug)]
struct Key {
    pool: Arc<()>,
    slot: usize,
    serial: u64,
}

/// Exclusive protocol permission to submit a destination write.
///
/// Dropping the permit does not return the slot to the pool.
///
/// ```compile_fail
/// use pronk_gpu::output_pool::WritePermit;
/// fn duplicate(permit: WritePermit) -> (WritePermit, WritePermit) {
///     (permit, permit)
/// }
/// ```
#[derive(Debug)]
pub struct WritePermit(Key);

/// Successful native producer completion permits one publication attempt.
#[derive(Debug)]
pub struct PublishPermit(Key);

/// Ownership handed to the transport, including an unacknowledged handoff.
///
/// Keep this handle until the transport reports release or is quiesced.
#[derive(Debug)]
pub struct Publication(Key);

impl Publication {
    pub fn slot(&self) -> usize {
        self.0.slot
    }
}

#[derive(Debug, Clone, Copy)]
enum WaitKind {
    Readers,
    Producer,
}

/// An independent native wait; it does not borrow the pool or its event loop.
#[derive(Debug)]
pub struct PendingAccess {
    key: Key,
    kind: WaitKind,
    fence: SyncFile,
    // Keep the allocation descriptor alive even if the pool is dropped.
    _buffer: OwnedFd,
}

/// A result tied to one pool, slot, use, and kind of native access.
#[derive(Debug)]
pub struct FinishedAccess {
    key: Key,
    kind: WaitKind,
    result: io::Result<Completion>,
}

impl PendingAccess {
    /// Cancellation leaves the slot pending; it is not a reuse operation.
    pub async fn wait(self) -> FinishedAccess {
        let Self {
            key,
            kind,
            fence,
            _buffer: buffer,
        } = self;
        let result = fence.wait().await;
        drop(buffer);
        FinishedAccess { key, kind, result }
    }
}

#[derive(Debug)]
pub enum AccessReady {
    Writable { slot: usize },
    Publish(PublishPermit),
}

/// Owns exported storage for a single immutable set of authorized recipients.
///
/// Transport release must exclude further consumer submissions before starting
/// reader preparation. The owner must keep that exclusion through its write
/// submission and completion enrollment. Native ioctls do not enforce it.
/// Graphics command/image resources remain the executor's responsibility;
/// dropping the pool or a pending wait does not cancel submitted native work.
#[derive(Debug)]
pub struct OutputPool {
    identity: Arc<()>,
    slots: Vec<Slot>,
}

impl OutputPool {
    /// Take a bounded set of distinct allocations; every slot starts unavailable.
    ///
    /// These must be exclusively controlled output allocations. A new pool or
    /// fresh protocol identity does not revoke previous exports of the storage.
    pub fn new(buffers: Vec<OwnedFd>) -> io::Result<Self> {
        if buffers.is_empty() || buffers.len() > MAX_OUTPUT_BUFFERS {
            return Err(invalid("invalid output pool size"));
        }
        let mut identities = Vec::with_capacity(buffers.len());
        for buffer in &buffers {
            use std::os::fd::AsRawFd;
            let stat = nix::sys::stat::fstat(buffer.as_raw_fd())?;
            let identity = (stat.st_dev, stat.st_ino);
            if identities.contains(&identity) {
                return Err(invalid("duplicate output allocation"));
            }
            identities.push(identity);
        }
        Ok(Self {
            identity: Arc::new(()),
            slots: buffers
                .into_iter()
                .map(|buffer| Slot {
                    buffer,
                    state: State::Unprepared,
                    serial: 0,
                })
                .collect(),
        })
    }

    pub fn state(&self, slot: usize) -> Option<State> {
        self.slots.get(slot).map(|slot| slot.state)
    }

    /// Duplicate storage for transport registration, never as a source grant.
    ///
    /// Registration must not itself cause consumer access. Only publication
    /// transfers permission to read. This method does not make storage revocable.
    pub fn export(&self, slot: usize) -> io::Result<OwnedFd> {
        self.slots
            .get(slot)
            .ok_or_else(|| invalid("unknown output slot"))?
            .buffer
            .try_clone()
    }

    /// Establish initial native readiness without admitting a source read.
    pub fn prepare_initial(&mut self, slot: usize) -> io::Result<PendingAccess> {
        let entry = self
            .slots
            .get(slot)
            .ok_or_else(|| invalid("unknown output slot"))?;
        if entry.state != State::Unprepared {
            return Err(invalid("output slot already prepared"));
        }
        let key = Key {
            pool: self.identity.clone(),
            slot,
            serial: entry.serial,
        };
        self.readers(key)
    }

    pub fn claim(&mut self, slot: usize) -> io::Result<WritePermit> {
        let entry = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| invalid("unknown output slot"))?;
        if entry.state != State::Writable {
            return Err(invalid("output slot not writable"));
        }
        let Some(serial) = entry.serial.checked_add(1) else {
            entry.state = State::Quarantined;
            return Err(invalid("output use identity exhausted"));
        };
        entry.serial = serial;
        entry.state = State::Writing;
        Ok(WritePermit(Key {
            pool: self.identity.clone(),
            slot,
            serial,
        }))
    }

    pub fn write_buffer(&self, permit: &WritePermit) -> io::Result<BorrowedFd<'_>> {
        self.check(&permit.0, State::Writing)?;
        Ok(self.slots[permit.0.slot].buffer.as_fd())
    }

    /// Enroll actual submitted write completion before allowing publication.
    ///
    /// On failure the slot is quarantined, even if work already reached the GPU.
    pub fn submitted(&mut self, permit: WritePermit, fence: SyncFile) -> io::Result<PendingAccess> {
        self.check(&permit.0, State::Writing)?;
        let slot = &mut self.slots[permit.0.slot];
        slot.state = State::Quarantined;
        import_completion(slot.buffer.as_fd(), Access::Write, &fence)?;
        let buffer = slot.buffer.try_clone()?;
        slot.state = State::WaitingForProducer;
        Ok(PendingAccess {
            key: permit.0,
            kind: WaitKind::Producer,
            fence,
            _buffer: buffer,
        })
    }

    /// Mark ownership transferred before attempting the transport handoff.
    ///
    /// If its acknowledgement is lost, retain the returned handle until release
    /// or transport shutdown establishes that no more reads may be submitted.
    ///
    /// ```compile_fail
    /// use pronk_gpu::output_pool::{OutputPool, WritePermit};
    /// fn premature(pool: &mut OutputPool, permit: WritePermit) {
    ///     pool.publish(permit).unwrap();
    /// }
    /// ```
    pub fn publish(&mut self, permit: PublishPermit) -> io::Result<Publication> {
        self.check(&permit.0, State::ReadyToPublish)?;
        self.slots[permit.0.slot].state = State::Published;
        Ok(Publication(permit.0))
    }

    /// Called only after transport retention ends; native reads may remain.
    pub fn returned(&mut self, publication: Publication) -> io::Result<PendingAccess> {
        self.check(&publication.0, State::Published)?;
        self.readers(publication.0)
    }

    /// Apply a native result without treating failure as reusable storage.
    pub fn complete(&mut self, finished: FinishedAccess) -> io::Result<AccessReady> {
        let expected = match finished.kind {
            WaitKind::Readers => State::WaitingForReaders,
            WaitKind::Producer => State::WaitingForProducer,
        };
        self.check(&finished.key, expected)?;
        let slot = &mut self.slots[finished.key.slot];
        slot.state = State::Quarantined;
        match finished.result? {
            Completion::Success => {}
            Completion::Failed(error) => {
                return Err(io::Error::from_raw_os_error(
                    error.checked_neg().unwrap_or(nix::libc::EIO),
                ))
            }
        }
        match finished.kind {
            WaitKind::Readers => {
                slot.state = State::Writable;
                Ok(AccessReady::Writable {
                    slot: finished.key.slot,
                })
            }
            WaitKind::Producer => {
                slot.state = State::ReadyToPublish;
                Ok(AccessReady::Publish(PublishPermit(finished.key)))
            }
        }
    }

    fn readers(&mut self, key: Key) -> io::Result<PendingAccess> {
        let slot = &mut self.slots[key.slot];
        slot.state = State::Quarantined;
        let fence = export_dependencies(slot.buffer.as_fd(), Access::ReadWrite)?;
        let buffer = slot.buffer.try_clone()?;
        slot.state = State::WaitingForReaders;
        Ok(PendingAccess {
            key,
            kind: WaitKind::Readers,
            fence,
            _buffer: buffer,
        })
    }

    fn check(&self, key: &Key, expected: State) -> io::Result<()> {
        if !Arc::ptr_eq(&key.pool, &self.identity) {
            return Err(invalid("output handle belongs to another pool"));
        }
        let slot = self
            .slots
            .get(key.slot)
            .ok_or_else(|| invalid("unknown output slot"))?;
        if slot.serial != key.serial || slot.state != expected {
            return Err(invalid("stale output handle or invalid state"));
        }
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
