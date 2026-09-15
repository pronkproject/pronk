//! Application-side correlation of GPU destination uses with PipeWire events.
//!
//! The transport actor owns its loop; this owner holds publication handles.
//! Native waits returned here must be driven outside the PipeWire loop and
//! without holding compositor-source leases.

use std::io;
use std::num::NonZeroU32;
use std::os::fd::{BorrowedFd, OwnedFd};

use pronk_dmabuf::SyncFile;
use pronk_gpu::output_pool::{
    AccessReady, FinishedAccess, OutputPool, PendingAccess, Publication, PublishPermit, State,
    WritePermit,
};
use pronk_pipewire::{
    PipeWireBufferTransport, VideoFrame, VideoNodeIdentity, VideoSourceActorEvent,
    VideoSourceStopReport,
};

#[derive(Debug)]
struct TransportSlot {
    id: NonZeroU32,
    initialized: bool,
    publication: Option<(u64, Publication)>,
}

#[derive(Debug)]
pub enum OutputEvent {
    Ignored,
    Wait(PendingAccess),
    Stopped(OutputRetirement),
}

#[derive(Debug)]
pub enum OutputReady {
    Writable(NonZeroU32),
    Publish(PublishPermit),
}

/// All locally retained publications are accounted for after source shutdown.
#[derive(Debug)]
pub struct OutputRetirement {
    pub waits: Vec<PendingAccess>,
    /// Failed snapshots leave the corresponding pool slots quarantined.
    pub errors: Vec<io::Error>,
}

/// One immutable media generation, including outstanding transport handoffs.
///
/// Frame sequences must increase strictly across publications in the generation.
/// They identify uses in release events; timestamps do not establish ownership.
#[derive(Debug)]
pub struct GpuOutput {
    identity: VideoNodeIdentity,
    pool: OutputPool,
    slots: Vec<TransportSlot>,
    last_sequence: Option<u64>,
    stopped: bool,
}

impl GpuOutput {
    /// `ids` maps pool slot order to the registered PipeWire buffer identifiers.
    pub fn new(
        identity: VideoNodeIdentity,
        pool: OutputPool,
        ids: Vec<NonZeroU32>,
    ) -> io::Result<Self> {
        if ids.is_empty() || pool.state(ids.len() - 1).is_none() || pool.state(ids.len()).is_some()
        {
            return Err(invalid("output registration does not match pool size"));
        }
        for (slot, id) in ids.iter().enumerate() {
            if ids[..slot].contains(id) || pool.state(slot) != Some(State::Unprepared) {
                return Err(invalid("output registration is duplicate or already used"));
            }
        }
        Ok(Self {
            identity,
            pool,
            slots: ids
                .into_iter()
                .map(|id| TransportSlot {
                    id,
                    initialized: false,
                    publication: None,
                })
                .collect(),
            last_sequence: None,
            stopped: false,
        })
    }

    pub fn export(&self, id: NonZeroU32) -> io::Result<OwnedFd> {
        self.pool.export(self.index(id)?)
    }

    pub fn claim(&mut self, id: NonZeroU32) -> io::Result<WritePermit> {
        self.running()?;
        self.pool.claim(self.index(id)?)
    }

    pub fn write_buffer(&self, permit: &WritePermit) -> io::Result<BorrowedFd<'_>> {
        self.running()?;
        self.pool.write_buffer(permit)
    }

    pub fn submitted(&mut self, permit: WritePermit, fence: SyncFile) -> io::Result<PendingAccess> {
        // Enrollment remains necessary for work accepted before shutdown.
        self.pool.submitted(permit, fence)
    }

    pub fn complete(&mut self, finished: FinishedAccess) -> io::Result<OutputReady> {
        Ok(match self.pool.complete(finished)? {
            AccessReady::Writable { slot } => OutputReady::Writable(self.slots[slot].id),
            AccessReady::Publish(permit) => OutputReady::Publish(permit),
        })
    }

    /// Record ownership before passing the returned frame to the source actor.
    ///
    /// An error or cancellation of the actor's publish acknowledgement does not
    /// remove this record. Wait for its matching release or source shutdown.
    pub fn begin_publish(
        &mut self,
        permit: PublishPermit,
        frame: VideoFrame,
    ) -> io::Result<VideoFrame> {
        self.running()?;
        let slot = self.index(frame.buffer_id)?;
        if slot != permit.slot()
            || !self.slots[slot].initialized
            || self.slots[slot].publication.is_some()
            || frame.acquire_point.is_some()
            || self
                .last_sequence
                .is_some_and(|last| frame.sequence <= last)
        {
            return Err(invalid("invalid GPU output publication"));
        }
        let publication = self.pool.publish(permit)?;
        self.slots[slot].publication = Some((frame.sequence, publication));
        self.last_sequence = Some(frame.sequence);
        Ok(frame)
    }

    pub fn handle_event(&mut self, event: &VideoSourceActorEvent) -> io::Result<OutputEvent> {
        let generation = match event {
            VideoSourceActorEvent::BufferAvailable {
                media_generation, ..
            }
            | VideoSourceActorEvent::BufferReleased {
                media_generation, ..
            } => *media_generation,
            VideoSourceActorEvent::GenerationFailed { identity, .. } => identity.media_generation,
        };
        if generation != self.identity.media_generation || self.stopped {
            return Ok(OutputEvent::Ignored);
        }
        match event {
            VideoSourceActorEvent::BufferAvailable {
                buffer_id,
                transport,
                ..
            } => {
                let slot = self.index(*buffer_id)?;
                if self.slots[slot].initialized
                    || *transport != PipeWireBufferTransport::ReadyBeforePublish
                {
                    return Err(invalid(
                        "GPU output requires one ready-before-publish availability event",
                    ));
                }
                self.slots[slot].initialized = true;
                Ok(OutputEvent::Wait(self.pool.prepare_initial(slot)?))
            }
            VideoSourceActorEvent::BufferReleased {
                buffer_id,
                sequence,
                ..
            } => {
                let slot = self.index(*buffer_id)?;
                if self.slots[slot].publication.as_ref().map(|(seq, _)| seq) != Some(sequence) {
                    return Err(invalid("release does not match the active GPU publication"));
                }
                let (_, publication) = self.slots[slot]
                    .publication
                    .take()
                    .expect("checked publication");
                Ok(OutputEvent::Wait(self.pool.returned(publication)?))
            }
            VideoSourceActorEvent::GenerationFailed { identity, .. } => {
                Ok(OutputEvent::Stopped(self.quiesced(identity)?))
            }
        }
    }

    /// Called only with the report obtained after joining the source loop.
    pub fn stopped(&mut self, report: &VideoSourceStopReport) -> io::Result<OutputRetirement> {
        self.quiesced(&report.identity)
    }

    fn quiesced(&mut self, identity: &VideoNodeIdentity) -> io::Result<OutputRetirement> {
        if *identity != self.identity || self.stopped {
            return Err(invalid(
                "shutdown report does not match the active GPU source",
            ));
        }
        self.stopped = true;
        let mut retirement = OutputRetirement {
            waits: Vec::new(),
            errors: Vec::new(),
        };
        // The actor may have queued release events that the application has not
        // consumed yet. Joining the entire source covers every local publication,
        // not only buffers still submitted in the actor's reclaim list.
        for slot in &mut self.slots {
            if let Some((_, publication)) = slot.publication.take() {
                match self.pool.returned(publication) {
                    Ok(wait) => retirement.waits.push(wait),
                    Err(error) => retirement.errors.push(error),
                }
            }
        }
        Ok(retirement)
    }

    fn index(&self, id: NonZeroU32) -> io::Result<usize> {
        self.slots
            .iter()
            .position(|slot| slot.id == id)
            .ok_or_else(|| invalid("unknown GPU output buffer"))
    }

    fn running(&self) -> io::Result<()> {
        if self.stopped {
            Err(invalid("GPU output generation is stopped"))
        } else {
            Ok(())
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use std::os::unix::net::UnixStream;

    fn identity() -> VideoNodeIdentity {
        VideoNodeIdentity {
            node_name: "gpu-output-test".into(),
            object_id: NonZeroU32::new(1).unwrap(),
            object_serial: NonZeroU64::new(2).unwrap(),
            media_generation: NonZeroU64::new(3).unwrap(),
        }
    }

    fn pool() -> OutputPool {
        let (socket, _peer) = UnixStream::pair().unwrap();
        OutputPool::new(vec![socket.into()]).unwrap()
    }

    #[test]
    fn registration_must_describe_the_exact_unused_pool() {
        let id = NonZeroU32::new(1).unwrap();
        assert!(GpuOutput::new(identity(), pool(), vec![]).is_err());
        assert!(GpuOutput::new(identity(), pool(), vec![id, id]).is_err());
        let mut used = pool();
        assert!(used.prepare_initial(0).is_err());
        assert!(GpuOutput::new(identity(), used, vec![id]).is_err());
        assert!(GpuOutput::new(identity(), pool(), vec![id]).is_ok());
    }

    #[test]
    fn foreign_events_do_not_initialize_storage() {
        let id = NonZeroU32::new(1).unwrap();
        let mut owner = GpuOutput::new(identity(), pool(), vec![id]).unwrap();
        let stale = VideoSourceActorEvent::BufferAvailable {
            media_generation: NonZeroU64::new(2).unwrap(),
            buffer_id: id,
            transport: PipeWireBufferTransport::ReadyBeforePublish,
        };
        assert!(matches!(
            owner.handle_event(&stale).unwrap(),
            OutputEvent::Ignored
        ));
        assert!(!owner.slots[0].initialized);
        assert!(owner.claim(id).is_err());
    }

    #[test]
    fn timeline_transport_is_not_accepted_as_ready_before_publish_transport() {
        let id = NonZeroU32::new(1).unwrap();
        let mut owner = GpuOutput::new(identity(), pool(), vec![id]).unwrap();
        let event = VideoSourceActorEvent::BufferAvailable {
            media_generation: identity().media_generation,
            buffer_id: id,
            transport: PipeWireBufferTransport::SyncTimeline,
        };
        assert!(owner.handle_event(&event).is_err());
        assert!(!owner.slots[0].initialized);
    }

    #[test]
    fn generation_failure_stops_new_claims() {
        let id = NonZeroU32::new(1).unwrap();
        let mut owner = GpuOutput::new(identity(), pool(), vec![id]).unwrap();
        let event = VideoSourceActorEvent::GenerationFailed {
            identity: identity(),
            error: pronk_pipewire::VideoSourceActorRuntimeError::EventStreamClosed,
            reclaimed_buffers: Box::new([]),
        };
        let OutputEvent::Stopped(retirement) = owner.handle_event(&event).unwrap() else {
            panic!("stop")
        };
        assert!(retirement.waits.is_empty());
        assert!(retirement.errors.is_empty());
        assert!(owner.claim(id).is_err());
        assert!(matches!(
            owner.handle_event(&event).unwrap(),
            OutputEvent::Ignored
        ));
    }
}
