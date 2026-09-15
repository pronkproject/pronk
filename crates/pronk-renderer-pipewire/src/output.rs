//! Publication ownership across asynchronous PipeWire handoff.

use std::io;
use std::num::NonZeroU32;

use pronk_pipewire::{
    PipeWireBufferTransport, VideoDamage, VideoFrame, VideoNodeIdentity, VideoSourceActorEvent,
    VideoSourceStopReport,
};
use pronk_renderer_worker::{OutputScope, PublishedOutput};

use crate::invalid;

struct Slot {
    available: bool,
    publication: Option<(u64, PublishedOutput)>,
}

/// Renderer publications retained for one PipeWire source generation.
pub struct TransportOutput {
    scope: OutputScope,
    layout: pronk_pipewire::VideoBufferLayout,
    identity: VideoNodeIdentity,
    slots: Vec<Slot>,
    next_sequence: Option<u64>,
    stopped: bool,
}

impl TransportOutput {
    pub(crate) fn new(
        scope: OutputScope,
        layout: pronk_pipewire::VideoBufferLayout,
        count: usize,
        identity: VideoNodeIdentity,
    ) -> Self {
        Self {
            scope,
            layout,
            identity,
            slots: (0..count)
                .map(|_| Slot {
                    available: false,
                    publication: None,
                })
                .collect(),
            next_sequence: Some(1),
            stopped: false,
        }
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        &self.identity
    }

    /// Retain pool ownership before returning a frame for PipeWire submission.
    pub fn begin_publish(
        &mut self,
        output: PublishedOutput,
        pts_ns: i64,
        discontinuity: bool,
    ) -> Result<VideoFrame, PublishError> {
        let slot = match self.validate_publication(&output) {
            Ok(slot) => slot,
            Err(error) => return Err(PublishError { output, error }),
        };
        let sequence = match self.next_sequence {
            Some(sequence) => sequence,
            None => {
                return Err(PublishError {
                    output,
                    error: invalid("renderer publication sequence exhausted"),
                });
            }
        };
        self.slots[slot].available = false;
        self.slots[slot].publication = Some((sequence, output));
        self.next_sequence = sequence.checked_add(1);
        Ok(VideoFrame {
            buffer_id: id(slot),
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

    fn validate_publication(&self, output: &PublishedOutput) -> io::Result<usize> {
        if self.stopped || !output.belongs_to(&self.scope) {
            return Err(invalid("publication belongs to another renderer output"));
        }
        let slot = output.slot();
        let entry = self
            .slots
            .get(slot)
            .ok_or_else(|| invalid("publication references an unknown buffer"))?;
        if !entry.available || entry.publication.is_some() {
            return Err(invalid("renderer buffer is unavailable for publication"));
        }
        Ok(slot)
    }

    /// Apply one source event and return ownership that needs renderer action.
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
                let slot = self.slot(*buffer_id)?;
                if slot.available
                    || slot.publication.is_some()
                    || *transport != PipeWireBufferTransport::Waited
                {
                    return Err(invalid("renderer received invalid buffer availability"));
                }
                slot.available = true;
                Ok(OutputEvent::Available {
                    slot: index(*buffer_id)?,
                })
            }
            VideoSourceActorEvent::BufferReleased {
                buffer_id,
                sequence,
                ..
            } => {
                let slot = self.slot(*buffer_id)?;
                if slot.publication.as_ref().map(|(current, _)| current) != Some(sequence) {
                    return Err(invalid("release does not match renderer publication"));
                }
                let (_, output) = slot.publication.take().expect("checked publication");
                slot.available = true;
                Ok(OutputEvent::Released(output))
            }
            VideoSourceActorEvent::GenerationFailed {
                identity,
                reclaimed_buffers,
                ..
            } => self.reclaim(identity, reclaimed_buffers),
        }
    }

    /// Reclaim submitted publications only after the source loop has stopped.
    pub fn stopped(&mut self, report: &VideoSourceStopReport) -> io::Result<OutputEvent> {
        self.reclaim(&report.identity, &report.reclaimed_buffers)
    }

    fn reclaim(
        &mut self,
        identity: &VideoNodeIdentity,
        reclaimed: &[NonZeroU32],
    ) -> io::Result<OutputEvent> {
        if *identity != self.identity || self.stopped {
            return Err(invalid("stop report belongs to another renderer output"));
        }
        let mut indices = Vec::with_capacity(reclaimed.len());
        for buffer in reclaimed {
            let slot = index(*buffer)?;
            if slot >= self.slots.len()
                || indices.contains(&slot)
                || self.slots[slot].publication.is_none()
            {
                return Err(invalid("stop report contains an invalid publication"));
            }
            indices.push(slot);
        }
        self.stopped = true;
        let outputs = self
            .slots
            .iter_mut()
            .filter_map(|slot| slot.publication.take().map(|(_, output)| output))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(OutputEvent::Reclaimed(outputs))
    }

    fn slot(&mut self, buffer: NonZeroU32) -> io::Result<&mut Slot> {
        let index = index(buffer)?;
        self.slots
            .get_mut(index)
            .ok_or_else(|| invalid("unknown renderer output buffer"))
    }
}

/// Transport event translated into renderer output ownership.
#[must_use = "handle availability and return released outputs to their pool"]
pub enum OutputEvent {
    Ignored,
    Available { slot: usize },
    Released(PublishedOutput),
    Reclaimed(Box<[PublishedOutput]>),
}

/// Failed publication setup retaining the pool's publication owner.
pub struct PublishError {
    output: PublishedOutput,
    error: io::Error,
}

impl PublishError {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (PublishedOutput, io::Error) {
        (self.output, self.error)
    }
}

fn index(buffer: NonZeroU32) -> io::Result<usize> {
    usize::try_from(buffer.get() - 1).map_err(|_| invalid("renderer buffer ID is too large"))
}

fn id(slot: usize) -> NonZeroU32 {
    NonZeroU32::new(u32::try_from(slot + 1).expect("bounded renderer output slot"))
        .expect("one-based renderer output slot")
}
