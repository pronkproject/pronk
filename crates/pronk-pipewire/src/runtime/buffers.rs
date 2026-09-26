//! PipeWire buffer binding, return, and frame publication callbacks.

use super::*;
use std::os::fd::AsRawFd;

use crate::model::BufferReturn;

pub(super) fn add_buffer(
    state: &mut ThreadState,
    raw: *mut pw::sys::pw_buffer,
) -> Result<(), VideoSourceRuntimeError> {
    let raw = NonNull::new(raw).ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
        "null pw_buffer",
    ))?;
    let buffer_id = state
        .tracker
        .next_unbound()
        .ok_or(VideoSourceRuntimeError::TooManyPipeWireBuffers)?;
    let runtime = state
        .buffer_mut(buffer_id)
        .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))?;
    let spa_buffer = unsafe { raw.as_ref().buffer };
    let spa_buffer = NonNull::new(spa_buffer).ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    let n_datas = unsafe { spa_buffer.as_ref().n_datas };
    let has_sync_meta = unsafe { sync_meta(spa_buffer.as_ptr()) }.is_some();
    let transport = match (
        n_datas,
        runtime.descriptor.timelines.as_ref(),
        has_sync_meta,
    ) {
        (1, _, _) => PipeWireBufferTransport::ReadyBeforePublish,
        (3, Some(_), true) => PipeWireBufferTransport::SyncTimeline,
        _ => {
            return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "invalid data/sync-timeline layout",
            ));
        }
    };

    unsafe { configure_spa_data(spa_buffer.as_ptr(), &runtime.descriptor, transport)? };
    state.tracker.bind(buffer_id, transport)?;
    state
        .buffer_mut(buffer_id)
        .expect("descriptor exists for an unbound buffer")
        .binding = Some(BufferBinding { raw, transport });
    Ok(())
}

pub(super) fn remove_buffer(
    state: &mut ThreadState,
    raw: *mut pw::sys::pw_buffer,
) -> Result<(), VideoSourceRuntimeError> {
    let buffer_id =
        state
            .buffer_id_for_raw(raw)
            .ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "remove names unknown pw_buffer",
            ))?;
    state.tracker.unbind(buffer_id)?;
    let runtime = state
        .buffer_mut(buffer_id)
        .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))?;
    runtime.binding = None;
    Ok(())
}

pub(super) unsafe fn configure_spa_data(
    spa_buffer: *mut spa::sys::spa_buffer,
    descriptor: &VideoBuffer,
    transport: PipeWireBufferTransport,
) -> Result<(), VideoSourceRuntimeError> {
    let buffer = unsafe { spa_buffer.as_mut() }.ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    if buffer.datas.is_null() || buffer.n_datas == 0 {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing data planes",
        ));
    }
    let datas = unsafe { std::slice::from_raw_parts_mut(buffer.datas, buffer.n_datas as usize) };
    if datas[0].chunk.is_null() {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing DMA-BUF chunk",
        ));
    }
    datas[0].type_ = spa::sys::SPA_DATA_DmaBuf;
    datas[0].flags = spa::sys::SPA_DATA_FLAG_READABLE;
    if descriptor.layout.storage == crate::VideoBufferStorage::MappableLinear {
        datas[0].flags |= spa::sys::SPA_DATA_FLAG_MAPPABLE;
    }
    datas[0].fd = descriptor.dma_buf.as_raw_fd() as i64;
    datas[0].mapoffset = 0;
    datas[0].maxsize = descriptor.layout.size.get() as u32;
    datas[0].data = std::ptr::null_mut();
    let chunk = unsafe { &mut *datas[0].chunk };
    chunk.offset = descriptor.layout.storage.offset();
    chunk.size = descriptor.layout.size.get() as u32 - chunk.offset;
    chunk.stride = descriptor.layout.pitch.get() as i32;

    if transport == PipeWireBufferTransport::SyncTimeline {
        let timelines =
            descriptor
                .timelines
                .as_ref()
                .ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                    "missing syncobj descriptors",
                ))?;
        if datas.len() != 3 {
            return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "sync timeline needs three data planes",
            ));
        }
        datas[1].type_ = spa::sys::SPA_DATA_SyncObj;
        datas[1].flags = spa::sys::SPA_DATA_FLAG_READABLE;
        datas[1].fd = timelines.ready.as_raw_fd() as i64;
        datas[1].mapoffset = 0;
        datas[1].maxsize = 0;
        datas[1].data = std::ptr::null_mut();
        datas[2].type_ = spa::sys::SPA_DATA_SyncObj;
        datas[2].flags = spa::sys::SPA_DATA_FLAG_READWRITE;
        datas[2].fd = timelines.reuse.as_raw_fd() as i64;
        datas[2].mapoffset = 0;
        datas[2].maxsize = 0;
        datas[2].data = std::ptr::null_mut();
        let sync = unsafe { sync_meta(spa_buffer) }.ok_or(
            VideoSourceRuntimeError::InvalidPipeWireBuffer("missing sync metadata"),
        )?;
        unsafe {
            *sync.as_ptr() = spa::sys::spa_meta_sync_timeline {
                flags: 0,
                padding: 0,
                acquire_point: 0,
                release_point: 0,
            };
        }
    }
    Ok(())
}

pub(super) fn process_returned_buffers(
    stream: &pw::stream::Stream,
    state: &mut ThreadState,
) -> Result<(), VideoSourceRuntimeError> {
    loop {
        let raw = unsafe { stream.dequeue_raw_buffer() };
        let Some(raw) = NonNull::new(raw) else {
            break;
        };
        let buffer_id = state.buffer_id_for_raw(raw.as_ptr()).ok_or(
            VideoSourceRuntimeError::InvalidPipeWireBuffer("dequeued unknown pw_buffer"),
        )?;
        let runtime = state
            .buffer(buffer_id)
            .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))?;
        let binding = runtime
            .binding
            .ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "dequeued unbound pw_buffer",
            ))?;
        let actual_release = match binding.transport {
            PipeWireBufferTransport::SyncTimeline => {
                let spa_buffer = unsafe { raw.as_ref().buffer };
                let sync = unsafe { sync_meta(spa_buffer) }.ok_or(
                    VideoSourceRuntimeError::InvalidPipeWireBuffer(
                        "returned buffer lost sync metadata",
                    ),
                )?;
                NonZeroU64::new(unsafe { sync.as_ref().release_point })
            }
            PipeWireBufferTransport::ReadyBeforePublish => None,
        };
        let event = match state.tracker.returned(buffer_id, actual_release)? {
            BufferReturn::Initial {
                buffer_id,
                transport,
            } => VideoSourceEvent::BufferAvailable {
                buffer_id,
                transport,
            },
            BufferReturn::Released {
                buffer_id,
                sequence,
            } => VideoSourceEvent::BufferReleased {
                buffer_id,
                sequence,
            },
            BufferReturn::Stale => continue,
        };
        state.emit(event)?;
    }
    Ok(())
}

pub(super) fn publish_frame(
    stream: &pw::stream::Stream,
    state: &mut ThreadState,
    frame: VideoFrame,
) -> Result<(), VideoSourceRuntimeError> {
    state.tracker.publish(frame)?;
    let runtime = state
        .buffer(frame.buffer_id)
        .ok_or(VideoSourceRuntimeError::UnknownBuffer(
            frame.buffer_id.get(),
        ))?;
    let binding = runtime
        .binding
        .ok_or(VideoSourceRuntimeError::InvalidOwnership(
            frame.buffer_id.get(),
        ))?;
    unsafe {
        fill_frame(
            binding.raw.as_ptr(),
            &runtime.descriptor,
            binding.transport,
            frame,
        )?
    };
    let result =
        unsafe { pw::sys::pw_stream_queue_buffer(stream.as_raw_ptr(), binding.raw.as_ptr()) };
    if result < 0 {
        return Err(VideoSourceRuntimeError::PipeWire(format!(
            "queue PipeWire buffer returned {result}"
        )));
    }
    Ok(())
}

pub(super) unsafe fn fill_frame(
    raw: *mut pw::sys::pw_buffer,
    descriptor: &VideoBuffer,
    transport: PipeWireBufferTransport,
    frame: VideoFrame,
) -> Result<(), VideoSourceRuntimeError> {
    let pipewire_buffer = unsafe { raw.as_mut() }.ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null pw_buffer"),
    )?;
    let spa_buffer = pipewire_buffer.buffer;
    let buffer = unsafe { spa_buffer.as_mut() }.ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    unsafe { fill_optional_frame_metadata(spa_buffer, frame)? };

    if transport == PipeWireBufferTransport::SyncTimeline {
        let point = frame
            .acquire_point
            .ok_or(VideoSourceRuntimeError::MissingAcquirePoint(
                frame.buffer_id.get(),
            ))?;
        let sync = unsafe { sync_meta(spa_buffer) }.ok_or(
            VideoSourceRuntimeError::InvalidPipeWireBuffer("missing sync metadata"),
        )?;
        unsafe {
            *sync.as_ptr() = spa::sys::spa_meta_sync_timeline {
                flags: spa::sys::SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,
                padding: 0,
                acquire_point: point.get(),
                release_point: point.get(),
            };
        }
    }

    if buffer.datas.is_null() || buffer.n_datas == 0 {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing frame data plane",
        ));
    }
    let data = unsafe { &mut *buffer.datas };
    if data.chunk.is_null() {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing frame chunk",
        ));
    }
    let chunk = unsafe { &mut *data.chunk };
    chunk.offset = descriptor.layout.storage.offset();
    chunk.size = descriptor.layout.size.get() as u32 - chunk.offset;
    chunk.stride = descriptor.layout.pitch.get() as i32;
    Ok(())
}

pub(super) unsafe fn fill_optional_frame_metadata(
    buffer: *mut spa::sys::spa_buffer,
    frame: VideoFrame,
) -> Result<(), VideoSourceRuntimeError> {
    // ParamMeta values are offers. A consumer may omit Header and VideoDamage
    // while still accepting the DMA-BUF data plane, so populate either one
    // when present without making it a transport prerequisite.
    let header = unsafe {
        spa::sys::spa_buffer_find_meta_data(
            buffer,
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        )
    }
    .cast::<spa::sys::spa_meta_header>();
    if let Some(header) = unsafe { header.as_mut() } {
        header.flags = if frame.discontinuity {
            spa::sys::SPA_META_HEADER_FLAG_DISCONT
        } else {
            0
        };
        header.offset = 0;
        header.pts = frame.pts_ns;
        header.dts_offset = 0;
        header.seq = frame.sequence;
    }

    let damage_meta =
        unsafe { spa::sys::spa_buffer_find_meta(buffer, spa::sys::SPA_META_VideoDamage) };
    let Some(damage_meta) = (unsafe { damage_meta.as_mut() }) else {
        return Ok(());
    };
    if damage_meta.data.is_null()
        || damage_meta.size < std::mem::size_of::<spa::sys::spa_meta_region>() as u32
    {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "short damage metadata",
        ));
    }
    let region_count = damage_meta.size as usize / std::mem::size_of::<spa::sys::spa_meta_region>();
    let regions = unsafe {
        std::slice::from_raw_parts_mut(
            damage_meta.data.cast::<spa::sys::spa_meta_region>(),
            region_count,
        )
    };
    for region in regions.iter_mut() {
        region.region = spa::sys::spa_region {
            position: spa::sys::spa_point { x: 0, y: 0 },
            size: spa::sys::spa_rectangle {
                width: 0,
                height: 0,
            },
        };
    }
    regions[0].region = spa::sys::spa_region {
        position: spa::sys::spa_point {
            x: frame.damage.x as i32,
            y: frame.damage.y as i32,
        },
        size: spa::sys::spa_rectangle {
            width: frame.damage.width.get(),
            height: frame.damage.height.get(),
        },
    };
    Ok(())
}

unsafe fn sync_meta(
    buffer: *mut spa::sys::spa_buffer,
) -> Option<NonNull<spa::sys::spa_meta_sync_timeline>> {
    if buffer.is_null() {
        return None;
    }
    let meta = unsafe {
        spa::sys::spa_buffer_find_meta_data(
            buffer,
            spa::sys::SPA_META_SyncTimeline,
            std::mem::size_of::<spa::sys::spa_meta_sync_timeline>(),
        )
    };
    NonNull::new(meta.cast())
}
