use std::num::{NonZeroU32, NonZeroU64};

use crate::{
    PipeWireBufferTransport, VideoBuffer, VideoBufferLayout, VideoFrame, VideoSourceRuntimeError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Unbound,
    ServerOwned {
        frame_submitted: bool,
        expected_release: Option<NonZeroU64>,
    },
    Available,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    id: NonZeroU32,
    layout: VideoBufferLayout,
    has_timelines: bool,
    transport: Option<PipeWireBufferTransport>,
    ownership: Ownership,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BufferReturn {
    Initial {
        buffer_id: NonZeroU32,
        transport: PipeWireBufferTransport,
    },
    Released {
        buffer_id: NonZeroU32,
    },
}

#[derive(Debug)]
pub(crate) struct BufferTracker {
    slots: Vec<Slot>,
}

impl BufferTracker {
    pub(crate) fn new(buffers: &[VideoBuffer]) -> Self {
        Self {
            slots: buffers
                .iter()
                .map(|buffer| Slot {
                    id: buffer.id,
                    layout: buffer.layout,
                    has_timelines: buffer.timelines.is_some(),
                    transport: None,
                    ownership: Ownership::Unbound,
                })
                .collect(),
        }
    }

    pub(crate) fn next_unbound(&self) -> Option<NonZeroU32> {
        self.slots
            .iter()
            .find(|slot| slot.ownership == Ownership::Unbound)
            .map(|slot| slot.id)
    }

    pub(crate) fn bind(
        &mut self,
        buffer_id: NonZeroU32,
        transport: PipeWireBufferTransport,
    ) -> Result<(), VideoSourceRuntimeError> {
        let slot = self.slot_mut(buffer_id)?;
        if slot.ownership != Ownership::Unbound
            || (transport == PipeWireBufferTransport::SyncTimeline && !slot.has_timelines)
        {
            return Err(VideoSourceRuntimeError::InvalidOwnership(buffer_id.get()));
        }
        slot.transport = Some(transport);
        slot.ownership = Ownership::ServerOwned {
            frame_submitted: false,
            expected_release: None,
        };
        Ok(())
    }

    pub(crate) fn unbind(&mut self, buffer_id: NonZeroU32) -> Result<(), VideoSourceRuntimeError> {
        let slot = self.slot_mut(buffer_id)?;
        slot.transport = None;
        slot.ownership = Ownership::Unbound;
        Ok(())
    }

    pub(crate) fn publish(&mut self, frame: VideoFrame) -> Result<(), VideoSourceRuntimeError> {
        let slot = self.slot_mut(frame.buffer_id)?;
        if slot.ownership != Ownership::Available {
            return Err(VideoSourceRuntimeError::InvalidOwnership(
                frame.buffer_id.get(),
            ));
        }
        if !frame.damage.is_bounded_by(slot.layout) {
            return Err(VideoSourceRuntimeError::InvalidDamage(
                frame.buffer_id.get(),
            ));
        }
        let transport = slot
            .transport
            .ok_or(VideoSourceRuntimeError::InvalidOwnership(
                frame.buffer_id.get(),
            ))?;
        match (transport, frame.acquire_point) {
            (PipeWireBufferTransport::SyncTimeline, None) => {
                return Err(VideoSourceRuntimeError::MissingAcquirePoint(
                    frame.buffer_id.get(),
                ));
            }
            (PipeWireBufferTransport::Waited, Some(_)) => {
                return Err(VideoSourceRuntimeError::UnexpectedAcquirePoint(
                    frame.buffer_id.get(),
                ));
            }
            _ => {}
        }
        slot.ownership = Ownership::ServerOwned {
            frame_submitted: true,
            expected_release: frame.acquire_point,
        };
        Ok(())
    }

    pub(crate) fn returned(
        &mut self,
        buffer_id: NonZeroU32,
        actual_release: Option<NonZeroU64>,
    ) -> Result<BufferReturn, VideoSourceRuntimeError> {
        let slot = self.slot_mut(buffer_id)?;
        let (frame_submitted, expected_release) = match slot.ownership {
            Ownership::ServerOwned {
                frame_submitted,
                expected_release,
            } => (frame_submitted, expected_release),
            _ => return Err(VideoSourceRuntimeError::InvalidOwnership(buffer_id.get())),
        };
        if expected_release != actual_release {
            return Err(VideoSourceRuntimeError::ReleasePointMismatch {
                buffer_id: buffer_id.get(),
                expected: expected_release.map_or(0, NonZeroU64::get),
                actual: actual_release.map_or(0, NonZeroU64::get),
            });
        }
        slot.ownership = Ownership::Available;
        match (frame_submitted, slot.transport) {
            (false, Some(transport)) => Ok(BufferReturn::Initial {
                buffer_id,
                transport,
            }),
            (true, Some(_)) => Ok(BufferReturn::Released { buffer_id }),
            (_, None) => Err(VideoSourceRuntimeError::InvalidOwnership(buffer_id.get())),
        }
    }

    fn slot_mut(&mut self, buffer_id: NonZeroU32) -> Result<&mut Slot, VideoSourceRuntimeError> {
        self.slots
            .iter_mut()
            .find(|slot| slot.id == buffer_id)
            .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VideoDamage;

    fn nonzero32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nonzero64(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn layout() -> VideoBufferLayout {
        VideoBufferLayout {
            width: nonzero32(1920),
            height: nonzero32(1080),
            pitch: nonzero32(7680),
            size: nonzero64(8_294_400),
            modifier: 0,
        }
    }

    fn tracker(has_timelines: bool) -> BufferTracker {
        BufferTracker {
            slots: vec![Slot {
                id: nonzero32(7),
                layout: layout(),
                has_timelines,
                transport: None,
                ownership: Ownership::Unbound,
            }],
        }
    }

    fn frame(acquire_point: Option<NonZeroU64>) -> VideoFrame {
        VideoFrame {
            buffer_id: nonzero32(7),
            sequence: 12,
            pts_ns: 34,
            damage: VideoDamage {
                x: 0,
                y: 0,
                width: nonzero32(1920),
                height: nonzero32(1080),
            },
            discontinuity: false,
            acquire_point,
        }
    }

    #[test]
    fn sync_timeline_requires_and_round_trips_the_exact_point() {
        let mut tracker = tracker(true);
        tracker
            .bind(nonzero32(7), PipeWireBufferTransport::SyncTimeline)
            .unwrap();
        assert_eq!(
            tracker.returned(nonzero32(7), None).unwrap(),
            BufferReturn::Initial {
                buffer_id: nonzero32(7),
                transport: PipeWireBufferTransport::SyncTimeline,
            }
        );
        assert!(matches!(
            tracker.publish(frame(None)),
            Err(VideoSourceRuntimeError::MissingAcquirePoint(7))
        ));
        tracker.publish(frame(Some(nonzero64(5)))).unwrap();
        assert!(matches!(
            tracker.returned(nonzero32(7), Some(nonzero64(4))),
            Err(VideoSourceRuntimeError::ReleasePointMismatch { .. })
        ));
        assert_eq!(
            tracker.returned(nonzero32(7), Some(nonzero64(5))).unwrap(),
            BufferReturn::Released {
                buffer_id: nonzero32(7)
            }
        );
    }

    #[test]
    fn waited_transport_rejects_timeline_points() {
        let mut tracker = tracker(true);
        tracker
            .bind(nonzero32(7), PipeWireBufferTransport::Waited)
            .unwrap();
        tracker.returned(nonzero32(7), None).unwrap();
        assert!(matches!(
            tracker.publish(frame(Some(nonzero64(1)))),
            Err(VideoSourceRuntimeError::UnexpectedAcquirePoint(7))
        ));
        tracker.publish(frame(None)).unwrap();
        assert_eq!(
            tracker.returned(nonzero32(7), None).unwrap(),
            BufferReturn::Released {
                buffer_id: nonzero32(7)
            }
        );
    }

    #[test]
    fn ownership_rejects_duplicate_publish_and_out_of_bounds_damage() {
        let mut tracker = tracker(false);
        tracker
            .bind(nonzero32(7), PipeWireBufferTransport::Waited)
            .unwrap();
        tracker.returned(nonzero32(7), None).unwrap();
        let mut invalid = frame(None);
        invalid.damage.x = 1919;
        invalid.damage.width = nonzero32(2);
        assert!(matches!(
            tracker.publish(invalid),
            Err(VideoSourceRuntimeError::InvalidDamage(7))
        ));
        tracker.publish(frame(None)).unwrap();
        assert!(matches!(
            tracker.publish(frame(None)),
            Err(VideoSourceRuntimeError::InvalidOwnership(7))
        ));
    }
}
