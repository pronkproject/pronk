//! Keep completed capture frames until their exact PipeWire uses are released.
//!
//! Neither the transport nor these storage handles receive capture authority.
//! Consumers finish CPU reads and enroll native reads before returning buffers.
//! The capture actor snapshots native reuse dependencies before the next write.

mod video;
pub use video::{State, Video};

use std::io;
use std::num::NonZeroU32;
use std::os::fd::AsFd;

use pronk_capture::{Actor, BufferHandle, BufferStorage, Frame, Layout};
use pronk_pipewire::{
    PipeWireBufferTransport, VideoBuffer, VideoBufferLayout, VideoBufferStorage, VideoDamage,
    VideoFrame, VideoNodeIdentity, VideoPixelFormat, VideoSourceActorEvent, VideoSourceStopReport,
    MAX_VIDEO_BUFFERS, MIN_VIDEO_BUFFERS,
};

/// One uniform authorized capture pool before its PipeWire node has started.
///
/// Storage metadata distinguishes CPU-mappable linear memory from an explicit
/// graphics modifier, including explicit modifier zero.
pub struct Registration {
    buffers: Vec<BufferHandle>,
    layout: Layout,
    video_layout: VideoBufferLayout,
}

impl Registration {
    pub fn new<F: AsFd + Send + 'static>(actor: &Actor<F>) -> io::Result<Self> {
        if !(MIN_VIDEO_BUFFERS..=MAX_VIDEO_BUFFERS).contains(&actor.buffers().len()) {
            return Err(invalid("capture pool is outside PipeWire buffer limits"));
        }
        let description = actor.buffers()[0].description();
        if actor
            .buffers()
            .iter()
            .any(|buffer| buffer.description() != description)
        {
            return Err(invalid("PipeWire requires a uniform capture layout"));
        }
        let layout = actor.layout();
        Ok(Self {
            buffers: actor.buffers().to_vec(),
            layout,
            video_layout: describe_video_layout(layout, description)?,
        })
    }

    pub fn video_layout(&self) -> VideoBufferLayout {
        self.video_layout
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
                    layout: self.video_layout,
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

fn describe_video_layout(
    layout: Layout,
    description: pronk_capture::BufferDescription,
) -> io::Result<VideoBufferLayout> {
    let format = match description.format {
        value if value == u32::from_le_bytes(*b"XR24") => VideoPixelFormat::Xrgb8888,
        value if value == u32::from_le_bytes(*b"AR24") => VideoPixelFormat::Argb8888,
        _ => {
            return Err(invalid(
                "PipeWire does not support the capture pixel format",
            ))
        }
    };
    let storage = match description.storage {
        BufferStorage::MappableLinear => VideoBufferStorage::MappableLinear,
        BufferStorage::DrmModifier { modifier, offset } => {
            VideoBufferStorage::DrmModifier { modifier, offset }
        }
    };
    Ok(VideoBufferLayout {
        format,
        width: layout.width,
        height: layout.height,
        pitch: description.pitch,
        size: description.size,
        storage,
    })
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
                if slot.initialized || *transport != PipeWireBufferTransport::ReadyBeforePublish {
                    return Err(invalid(
                        "capture requires one ready-before-publish availability event",
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

#[cfg(test)]
mod tests {
    use super::*;
    use pronk_capture::{BufferDescription, BufferStorage};
    use std::num::{NonZeroU32, NonZeroU64};

    fn nz(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    #[test]
    fn graphics_storage_keeps_its_explicit_linear_modifier() {
        let layout = describe_video_layout(
            Layout {
                width: nz(1920),
                height: nz(1080),
            },
            BufferDescription {
                format: u32::from_le_bytes(*b"XR24"),
                pitch: nz(7680),
                size: NonZeroU64::new(8_294_400).unwrap(),
                storage: BufferStorage::DrmModifier {
                    modifier: 0,
                    offset: 0,
                },
            },
        )
        .unwrap();

        assert_eq!(layout.format, VideoPixelFormat::Xrgb8888);
        assert_eq!(
            layout.storage,
            VideoBufferStorage::DrmModifier {
                modifier: 0,
                offset: 0
            }
        );
    }

    #[test]
    fn unsupported_capture_formats_do_not_reach_pipewire() {
        let error = describe_video_layout(
            Layout {
                width: nz(1),
                height: nz(1),
            },
            BufferDescription {
                format: u32::from_le_bytes(*b"RG16"),
                pitch: nz(2),
                size: NonZeroU64::new(2).unwrap(),
                storage: BufferStorage::MappableLinear,
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
