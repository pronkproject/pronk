//! Video format and buffer-parameter negotiation for the source loop.

use super::*;
use std::io::Cursor;

use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFlags, VideoFormat, VideoInfoRaw};
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

pub(super) fn format_parameter(
    frame_rate: crate::VideoFrameRate,
    layout: crate::VideoBufferLayout,
) -> Result<Vec<u8>, VideoSourceRuntimeError> {
    // Only the CPU-copy profile omits the modifier. Explicit GPU layouts keep
    // their memory:DMABuf negotiation, including an explicitly linear image.
    let mut object = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Id,
            pixel_format(layout.format)
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: layout.width.get(),
                height: layout.height.get(),
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction {
                num: frame_rate.numerator().get(),
                denom: frame_rate.denominator().get(),
            }
        ),
    );
    if let crate::VideoBufferStorage::DrmModifier { modifier, .. } = layout.storage {
        object.properties.push(spa::pod::property!(
            FormatProperties::VideoModifier,
            Long,
            modifier as i64
        ));
    }
    serialize_value(&Value::Object(object))
}

pub(super) fn classify_format_change(param: Option<&Pod>) -> FormatChange<'_> {
    match param {
        Some(param) => FormatChange::Negotiated(param),
        None => FormatChange::Cleared,
    }
}

pub(super) fn negotiate_buffers(
    stream: &pw::stream::Stream,
    state: &ThreadState,
    format: &Pod,
) -> Result<(), VideoSourceRuntimeError> {
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(format)
        .map_err(|_| VideoSourceRuntimeError::UnsupportedFormat)?;
    let mut info = VideoInfoRaw::new();
    info.parse(format)
        .map_err(|_| VideoSourceRuntimeError::UnsupportedFormat)?;
    let layout = state.buffers[0].descriptor.layout;
    if media_type != MediaType::Video
        || media_subtype != MediaSubtype::Raw
        || info.format() != pixel_format(layout.format)
        || !storage_matches(&info, layout.storage)
        || info.size().width != layout.width.get()
        || info.size().height != layout.height.get()
        || !state
            .config
            .frame_rate
            .matches_fraction(info.framerate().num, info.framerate().denom)
    {
        return Err(VideoSourceRuntimeError::UnsupportedFormat);
    }

    let values = buffer_parameters(state)?;
    let mut pods = values
        .iter()
        .map(|value| {
            Pod::from_bytes(value).ok_or_else(|| {
                VideoSourceRuntimeError::PipeWire("serialize PipeWire buffer parameter".to_string())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    stream
        .update_params(&mut pods)
        .map_err(|error| pipewire_error("update source buffer parameters", error))
}

pub(super) fn pixel_format(format: crate::VideoPixelFormat) -> VideoFormat {
    match format {
        crate::VideoPixelFormat::Xrgb8888 => VideoFormat::BGRx,
        crate::VideoPixelFormat::Argb8888 => VideoFormat::BGRA,
        crate::VideoPixelFormat::Xbgr8888 => VideoFormat::RGBx,
        crate::VideoPixelFormat::Abgr8888 => VideoFormat::RGBA,
    }
}

pub(super) fn storage_matches(info: &VideoInfoRaw, storage: crate::VideoBufferStorage) -> bool {
    match storage {
        crate::VideoBufferStorage::MappableLinear => !info.flags().contains(VideoFlags::MODIFIER),
        crate::VideoBufferStorage::DrmModifier { modifier, .. } => {
            info.flags().contains(VideoFlags::MODIFIER) && info.modifier() == modifier
        }
    }
}

fn buffer_parameters(state: &ThreadState) -> Result<Vec<Vec<u8>>, VideoSourceRuntimeError> {
    let layout = state.buffers[0].descriptor.layout;
    let count = i32::try_from(state.buffers.len()).expect("buffer count is bounded by 64");
    let size = i32::try_from(layout.size.get()).expect("validated layout fits i32-sized frame");
    let stride = i32::try_from(layout.pitch.get()).expect("validated pitch fits i32");
    let dma_buf_flag = 1i32
        .checked_shl(spa::sys::SPA_DATA_DmaBuf)
        .ok_or_else(|| VideoSourceRuntimeError::PipeWire("invalid DMA-BUF type".to_string()))?;
    let sync_meta_flag = 1i32
        .checked_shl(spa::sys::SPA_META_SyncTimeline)
        .ok_or_else(|| VideoSourceRuntimeError::PipeWire("invalid sync meta type".to_string()))?;
    let buffer_count = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: count,
            min: 2,
            max: count,
        },
    )));
    let data_type = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Flags {
            default: dma_buf_flag,
            flags: vec![dma_buf_flag],
        },
    )));
    let mut values = Vec::new();

    if state.buffers[0].descriptor.timelines.is_some() {
        let mut meta_type = Property::new(
            spa::sys::SPA_PARAM_BUFFERS_metaType,
            Value::Int(sync_meta_flag),
        );
        meta_type.flags = PropertyFlags::MANDATORY;
        values.push(Value::Object(Object {
            type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
            id: spa::sys::SPA_PARAM_Buffers,
            properties: vec![
                Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, buffer_count.clone()),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(3)),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, data_type.clone()),
                meta_type,
            ],
        }));
    }
    values.push(Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, buffer_count),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, data_type),
        ],
    }));
    values.push(meta_parameter(
        spa::sys::SPA_META_Header,
        std::mem::size_of::<spa::sys::spa_meta_header>(),
    ));
    values.push(meta_parameter(
        spa::sys::SPA_META_VideoDamage,
        std::mem::size_of::<spa::sys::spa_meta_region>(),
    ));
    if state.buffers[0].descriptor.timelines.is_some() {
        values.push(meta_parameter(
            spa::sys::SPA_META_SyncTimeline,
            std::mem::size_of::<spa::sys::spa_meta_sync_timeline>(),
        ));
    }

    values
        .iter()
        .map(serialize_value)
        .collect::<Result<Vec<_>, _>>()
}

fn meta_parameter(meta_type: u32, size: usize) -> Value {
    Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_META_type, Value::Id(Id(meta_type))),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(i32::try_from(size).expect("SPA metadata size fits i32")),
            ),
        ],
    })
}

fn serialize_value(value: &Value) -> Result<Vec<u8>, VideoSourceRuntimeError> {
    PodSerializer::serialize(Cursor::new(Vec::new()), value)
        .map(|(bytes, _)| bytes.into_inner())
        .map_err(|error| VideoSourceRuntimeError::PipeWire(format!("serialize SPA pod: {error:?}")))
}
