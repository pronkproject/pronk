//! Keep completed capture frames until their exact PipeWire uses are released.
//!
//! Neither the transport nor these storage handles receive capture authority.
//! Consumers finish CPU reads and enroll native reads before returning buffers.
//! The capture actor snapshots native reuse dependencies before the next write.

mod video;
pub use video::{State, Video};

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::AsFd;

use pronk_capture::{Actor, BufferHandle, Frame, Layout};
use pronk_pipewire::{
    PipeWireBufferTransport, VideoBuffer, VideoBufferLayout, VideoBufferStorage, VideoDamage,
    VideoFrame, VideoNodeIdentity, VideoPixelFormat, VideoSourceActorEvent, VideoSourceStopReport,
    MAX_VIDEO_BUFFERS, MIN_VIDEO_BUFFERS,
};

/// One CPU-mappable linear pool, before its PipeWire node has been started.
/// The allocator must support CPU mapping; modifier zero alone does not prove it.
pub struct Registration {
    buffers: Vec<BufferHandle>,
    layout: Layout,
}

impl Registration {
    pub fn new<F: AsFd + Send + 'static>(actor: &Actor<F>) -> io::Result<Self> {
        if !(MIN_VIDEO_BUFFERS..=MAX_VIDEO_BUFFERS).contains(&actor.buffers().len()) {
            return Err(invalid("capture pool is outside PipeWire buffer limits"));
        }
        if actor
            .buffers()
            .iter()
            .any(|buffer| buffer.stride() != actor.buffers()[0].stride())
        {
            return Err(invalid("PipeWire requires a uniform capture stride"));
        }
        Ok(Self {
            buffers: actor.buffers().to_vec(),
            layout: actor.layout(),
        })
    }

    /// Export only destinations, to the same authorized media recipient.
    /// No pixels may be accessed until a completed frame is published.
    pub fn export(&self) -> io::Result<Vec<VideoBuffer>> {
        self.buffers
            .iter()
            .enumerate()
            .map(|(index, buffer)| {
                Ok(VideoBuffer {
                    id: id(index),
                    dma_buf: buffer.as_fd().try_clone_to_owned()?,
                    timelines: None,
                    layout: VideoBufferLayout {
                        format: VideoPixelFormat::Xrgb8888,
                        width: self.layout.width,
                        height: self.layout.height,
                        pitch: buffer.stride(),
                        size: NonZeroU64::new(
                            u64::from(buffer.stride().get()) * u64::from(self.layout.height.get()),
                        )
                        .expect("nonzero layout"),
                        storage: VideoBufferStorage::MappableLinear,
                    },
                })
            })
            .collect()
    }

    /// Bind to the identity returned after starting the exported pool's node.
    pub fn bind(self, identity: VideoNodeIdentity) -> Output {
        Output {
            identity,
            layout: self.layout,
            slots: self
                .buffers
                .into_iter()
                .map(|buffer| Slot {
                    buffer,
                    initialized: false,
                    publication: None,
                })
                .collect(),
            next_sequence: Some(1),
            stopped: false,
        }
    }
}

struct Slot {
    buffer: BufferHandle,
    initialized: bool,
    publication: Option<(u64, Frame)>,
}

/// Publications retained independently of async publish acknowledgements.
///
/// Dropping the owner retires outstanding destinations instead of making them
/// writable. An error from publication or event handling does not erase an
/// earlier publication. Stop the source on errors before discarding the owner.
pub struct Output {
    identity: VideoNodeIdentity,
    layout: Layout,
    slots: Vec<Slot>,
    next_sequence: Option<u64>,
    stopped: bool,
}

impl Output {
    /// Retain ownership before sending the returned description to PipeWire.
    /// An acknowledgement failure is not permission to release the frame.
    pub fn begin_publish(
        &mut self,
        frame: Frame,
        pts_ns: i64,
        discontinuity: bool,
    ) -> io::Result<VideoFrame> {
        if self.stopped {
            return Err(invalid("capture output is stopped"));
        }
        let index = self
            .slots
            .iter()
            .position(|slot| slot.buffer.contains_frame(&frame))
            .ok_or_else(|| invalid("frame belongs to another capture pool"))?;
        let slot = &mut self.slots[index];
        if !slot.initialized || slot.publication.is_some() {
            return Err(invalid(
                "capture destination is not available for publication",
            ));
        }
        let sequence = self
            .next_sequence
            .ok_or_else(|| invalid("publication sequence exhausted"))?;
        slot.publication = Some((sequence, frame));
        self.next_sequence = sequence.checked_add(1);
        Ok(VideoFrame {
            buffer_id: id(index),
            sequence,
            pts_ns,
            damage: VideoDamage {
                x: 0,
                y: 0,
                width: self.layout.width,
                height: self.layout.height,
            },
            discontinuity,
            acquire_point: None,
        })
    }

    pub fn handle_event(&mut self, event: &VideoSourceActorEvent) -> io::Result<()> {
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
            return Ok(());
        }
        match event {
            VideoSourceActorEvent::BufferAvailable {
                buffer_id,
                transport,
                ..
            } => {
                let slot = self.slot(*buffer_id)?;
                if slot.initialized || *transport != PipeWireBufferTransport::Waited {
                    return Err(invalid(
                        "capture requires one waited-transport availability event",
                    ));
                }
                slot.initialized = true;
            }
            VideoSourceActorEvent::BufferReleased {
                buffer_id,
                sequence,
                ..
            } => {
                let slot = self.slot(*buffer_id)?;
                if slot.publication.as_ref().map(|(sequence, _)| sequence) != Some(sequence) {
                    return Err(invalid("release does not match the capture publication"));
                }
                drop(slot.publication.take());
            }
            VideoSourceActorEvent::GenerationFailed { identity, .. } => self.finish(identity)?,
        }
        Ok(())
    }

    pub fn stopped(&mut self, report: &VideoSourceStopReport) -> io::Result<()> {
        self.finish(&report.identity)
    }

    fn finish(&mut self, identity: &VideoNodeIdentity) -> io::Result<()> {
        if *identity != self.identity {
            return Err(invalid("stop report belongs to another capture output"));
        }
        self.stopped = true;
        self.retire();
        Ok(())
    }

    fn slot(&mut self, id: NonZeroU32) -> io::Result<&mut Slot> {
        self.slots
            .get_mut((id.get() - 1) as usize)
            .ok_or_else(|| invalid("unknown capture output buffer"))
    }

    fn retire(&mut self) {
        for slot in &mut self.slots {
            if let Some((_, frame)) = slot.publication.take() {
                frame.retire();
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.retire();
    }
}

fn id(index: usize) -> NonZeroU32 {
    NonZeroU32::new(index as u32 + 1).expect("bounded pool index")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
