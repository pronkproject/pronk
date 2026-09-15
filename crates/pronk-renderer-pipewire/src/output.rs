//! Publication ownership across asynchronous PipeWire handoff.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};

use pronk_pipewire::{
    PipeWireBufferTransport, VideoDamage, VideoFrame, VideoNodeIdentity, VideoSourceActorEvent,
    VideoSourceStopReport,
};
use pronk_renderer_worker::{
    CompletedOutput, CompletedReturn, FinishedOutput, OutputDestination, OutputPool, OutputReturn,
    OutputScope, PendingOutput, PrivateBuffer, PublishedOutput, ReadyOutput,
};

use crate::invalid;

struct Slot {
    available: bool,
    publication: Option<(u64, PublishedOutput)>,
}

/// Renderer publications retained for one PipeWire source generation.
struct TransportOutput {
    scope: OutputScope,
    layout: pronk_pipewire::VideoBufferLayout,
    identity: VideoNodeIdentity,
    slots: Vec<Slot>,
    next_sequence: Option<u64>,
    stopped: bool,
}

/// Native output access and PipeWire ownership for one media generation.
pub struct OutputSession {
    pool: OutputPool,
    transport: TransportOutput,
}

impl OutputSession {
    pub(crate) fn new(
        pool: OutputPool,
        layout: pronk_pipewire::VideoBufferLayout,
        identity: VideoNodeIdentity,
    ) -> Self {
        let transport = TransportOutput::new(pool.scope(), layout, pool.len(), identity);
        Self { pool, transport }
    }

    pub fn identity(&self) -> &VideoNodeIdentity {
        &self.transport.identity
    }

    /// Reserve storage only after PipeWire makes it available.
    pub fn claim(&mut self, slot: usize) -> io::Result<OutputDestination> {
        if !self.transport.available(slot)? {
            return Err(invalid("renderer output is unavailable to PipeWire"));
        }
        self.pool.claim(slot)
    }

    pub fn submit(&mut self, output: CompletedOutput) -> io::Result<PendingOutput> {
        self.pool.submit(output)
    }

    pub fn finish(&mut self, output: FinishedOutput) -> io::Result<ReadyOutput> {
        self.pool.finish(output)
    }

    /// Publish completed pixels while retaining their transport ownership.
    pub fn publish(
        &mut self,
        output: ReadyOutput,
        pts_ns: i64,
        discontinuity: bool,
    ) -> Result<(PrivateBuffer, OutputFrame), Box<PublishError>> {
        let (private, published) = self.pool.publish(output).map_err(|error| {
            Box::new(PublishError {
                private: None,
                retirement: None,
                error,
            })
        })?;
        let content_serial = published.content_serial();
        match self
            .transport
            .begin_publish(published, pts_ns, discontinuity)
        {
            Ok(frame) => Ok((
                private,
                OutputFrame {
                    frame,
                    content_serial,
                },
            )),
            Err(error) => {
                let (published, error) = error.into_parts();
                let retirement = self.pool.begin_return(published).ok();
                Err(Box::new(PublishError {
                    private: Some(private),
                    retirement,
                    error,
                }))
            }
        }
    }

    /// Apply a transport event without performing its native wait.
    pub fn handle_event(&mut self, event: &VideoSourceActorEvent) -> io::Result<OutputEvent> {
        let event = self.transport.handle_event(event)?;
        self.apply(event)
    }

    /// Reclaim publications after the source loop has stopped.
    pub fn stopped(&mut self, report: &VideoSourceStopReport) -> io::Result<OutputEvent> {
        let event = self.transport.stopped(report)?;
        self.apply(event)
    }

    /// Make a returned slot writable after its wait completes.
    pub fn finish_return(&mut self, returned: CompletedReturn) -> io::Result<usize> {
        self.pool.finish_return(returned)
    }

    fn apply(&mut self, event: TransportEvent) -> io::Result<OutputEvent> {
        match event {
            TransportEvent::Ignored => Ok(OutputEvent::Ignored),
            TransportEvent::Available { slot } => Ok(OutputEvent::Available { slot }),
            TransportEvent::Released(output) => {
                self.pool.begin_return(output).map(OutputEvent::Released)
            }
            TransportEvent::Reclaimed(outputs) => outputs
                .into_vec()
                .into_iter()
                .map(|output| self.pool.begin_return(output))
                .collect::<io::Result<Vec<_>>>()
                .map(|returns| OutputEvent::Reclaimed(returns.into_boxed_slice())),
        }
    }
}

/// PipeWire description for one output with its source-content identity.
#[must_use = "submit the frame to PipeWire or stop its output generation"]
pub struct OutputFrame {
    frame: VideoFrame,
    content_serial: Option<NonZeroU64>,
}

impl OutputFrame {
    pub fn content_serial(&self) -> Option<NonZeroU64> {
        self.content_serial
    }

    pub fn frame(&self) -> &VideoFrame {
        &self.frame
    }

    pub fn into_frame(self) -> VideoFrame {
        self.frame
    }
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

    /// Retain pool ownership before returning a frame for PipeWire submission.
    fn begin_publish(
        &mut self,
        output: PublishedOutput,
        pts_ns: i64,
        discontinuity: bool,
    ) -> Result<VideoFrame, TransportPublishError> {
        let slot = match self.validate_publication(&output) {
            Ok(slot) => slot,
            Err(error) => return Err(TransportPublishError::new(output, error)),
        };
        let sequence = match self.next_sequence {
            Some(sequence) => sequence,
            None => {
                return Err(TransportPublishError::new(
                    output,
                    invalid("renderer publication sequence exhausted"),
                ));
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
    fn handle_event(&mut self, event: &VideoSourceActorEvent) -> io::Result<TransportEvent> {
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
            return Ok(TransportEvent::Ignored);
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
                Ok(TransportEvent::Available {
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
                Ok(TransportEvent::Released(output))
            }
            VideoSourceActorEvent::GenerationFailed {
                identity,
                reclaimed_buffers,
                ..
            } => self.reclaim(identity, reclaimed_buffers),
        }
    }

    /// Reclaim submitted publications only after the source loop has stopped.
    fn stopped(&mut self, report: &VideoSourceStopReport) -> io::Result<TransportEvent> {
        self.reclaim(&report.identity, &report.reclaimed_buffers)
    }

    fn reclaim(
        &mut self,
        identity: &VideoNodeIdentity,
        reclaimed: &[NonZeroU32],
    ) -> io::Result<TransportEvent> {
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
        Ok(TransportEvent::Reclaimed(outputs))
    }

    fn slot(&mut self, buffer: NonZeroU32) -> io::Result<&mut Slot> {
        let index = index(buffer)?;
        self.slots
            .get_mut(index)
            .ok_or_else(|| invalid("unknown renderer output buffer"))
    }

    fn available(&self, slot: usize) -> io::Result<bool> {
        self.slots
            .get(slot)
            .map(|entry| entry.available && entry.publication.is_none())
            .ok_or_else(|| invalid("unknown renderer output buffer"))
    }
}

enum TransportEvent {
    Ignored,
    Available { slot: usize },
    Released(PublishedOutput),
    Reclaimed(Box<[PublishedOutput]>),
}

/// Transport event translated into renderer output ownership.
#[must_use = "handle availability and return released outputs to their pool"]
pub enum OutputEvent {
    Ignored,
    Available { slot: usize },
    Released(OutputReturn),
    Reclaimed(Box<[OutputReturn]>),
}

/// Failed publication setup and any ownership available for recovery.
pub struct PublishError {
    private: Option<PrivateBuffer>,
    retirement: Option<OutputReturn>,
    error: io::Error,
}

impl PublishError {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    /// Split the error into its private storage, output retirement, and cause.
    ///
    /// Missing storage was quarantined by the layer that rejected it. A
    /// returned retirement must finish before its output slot can be reused.
    pub fn into_parts(self) -> (Option<PrivateBuffer>, Option<OutputReturn>, io::Error) {
        (self.private, self.retirement, self.error)
    }
}

struct TransportPublishError {
    output: Box<PublishedOutput>,
    error: io::Error,
}

impl TransportPublishError {
    fn new(output: PublishedOutput, error: io::Error) -> Self {
        Self {
            output: Box::new(output),
            error,
        }
    }

    fn into_parts(self) -> (PublishedOutput, io::Error) {
        (*self.output, self.error)
    }
}

fn index(buffer: NonZeroU32) -> io::Result<usize> {
    usize::try_from(buffer.get() - 1).map_err(|_| invalid("renderer buffer ID is too large"))
}

fn id(slot: usize) -> NonZeroU32 {
    NonZeroU32::new(u32::try_from(slot + 1).expect("bounded renderer output slot"))
        .expect("one-based renderer output slot")
}
