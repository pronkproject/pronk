use std::num::NonZeroU64;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::h264;
use crate::model::{
    DrmVideoFormat, MediaGraphError, VideoCadence, VideoCodec, VideoEncoder, VideoFrameDependency,
    VideoInputLayout,
};
use crate::vp8;

impl VideoCodec {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::H264 => "H.264",
        }
    }

    pub(crate) fn output_caps(self) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Vp8 => vp8::encoder_output_caps(),
            Self::H264 => h264::encoder_output_caps(),
        }
    }

    pub(crate) fn build_parser(self) -> Result<Option<gst::Element>, MediaGraphError> {
        match self {
            Self::Vp8 => Ok(None),
            Self::H264 => gst::ElementFactory::make("h264parse")
                .name("pronk-h264-parser")
                .property("config-interval", -1_i32)
                .property("disable-passthrough", true)
                .build()
                .map(Some)
                .map_err(|error| MediaGraphError::new(format!("construct H.264 parser: {error}"))),
        }
    }

    pub(crate) fn validate_caps(self, caps: &gst::CapsRef) -> Result<(), MediaGraphError> {
        match self {
            Self::Vp8 => vp8::validate_caps(caps),
            Self::H264 => h264::validate_caps(caps),
        }
    }

    pub(crate) fn validate_frame(
        self,
        bytes: &[u8],
        dependency: VideoFrameDependency,
        first: bool,
    ) -> Result<(), MediaGraphError> {
        match self {
            Self::Vp8 => vp8::validate_frame(bytes, dependency, first),
            Self::H264 => h264::validate_access_unit(bytes, dependency, first),
        }
    }
}

impl VideoEncoder {
    /// Check the selected encoder path against concrete picture sizes.
    ///
    /// The converter's DMA-BUF input, VA output and encoder input must all
    /// accept the selected picture size and cadence.
    pub fn supported_dimensions(
        &self,
        dimensions: &[(u32, u32)],
        cadence: VideoCadence,
    ) -> Result<Vec<bool>, MediaGraphError> {
        let Self::VaH264 { .. } = self else {
            return Ok(vec![true; dimensions.len()]);
        };
        gst::init().map_err(|error| {
            MediaGraphError::new(format!(
                "initialize GStreamer while probing encoder sizes: {error}"
            ))
        })?;
        let converter = self.build_converter()?;
        let encoder = self.build(NonZeroU64::new(2_000_000).unwrap(), cadence)?;
        require_va_baseline_output(&encoder)?;
        let converter_output = converter
            .pad_template("src")
            .ok_or_else(|| MediaGraphError::new("VA converter has no source pad template"))?;
        let converter_input = converter
            .pad_template("sink")
            .ok_or_else(|| MediaGraphError::new("VA converter has no sink pad template"))?;
        let encoder_input = encoder
            .pad_template("sink")
            .ok_or_else(|| MediaGraphError::new("VA encoder has no sink pad template"))?;
        let compatible = converter_output.caps().intersect(encoder_input.caps());
        let input = supported_dma_buf_dimensions(converter_input.caps(), dimensions, cadence)?;
        let output = supported_va_dimensions(&compatible, dimensions, cadence)?;
        Ok(input
            .into_iter()
            .zip(output)
            .map(|(input, output)| input && output)
            .collect())
    }

    /// Query concrete DMA-BUF layouts accepted by the selected converter.
    pub fn supported_dma_buf_formats(
        &self,
        cadence: VideoCadence,
    ) -> Result<Vec<DrmVideoFormat>, MediaGraphError> {
        let Self::VaH264 { .. } = self else {
            return Ok(Vec::new());
        };
        gst::init().map_err(|error| {
            MediaGraphError::new(format!(
                "initialize GStreamer while probing encoder input: {error}"
            ))
        })?;
        let converter = self.build_converter()?;
        let encoder = self.build(NonZeroU64::new(2_000_000).unwrap(), cadence)?;
        require_va_baseline_output(&encoder)?;
        let template = converter
            .pad_template("sink")
            .ok_or_else(|| MediaGraphError::new("VA converter has no sink pad template"))?;
        let mut formats = Vec::new();
        for (format, _) in converter_dma_buf_formats(template.caps())? {
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
        Ok(formats)
    }

    /// Match each picture size against complete converter input tuples.
    pub fn supported_dma_buf_formats_for_dimensions(
        &self,
        dimensions: &[(u32, u32)],
        cadence: VideoCadence,
    ) -> Result<Vec<Vec<DrmVideoFormat>>, MediaGraphError> {
        let Self::VaH264 { .. } = self else {
            return Ok(vec![Vec::new(); dimensions.len()]);
        };
        let supported = self.supported_dimensions(dimensions, cadence)?;
        let converter = self.build_converter()?;
        let input = converter
            .pad_template("sink")
            .ok_or_else(|| MediaGraphError::new("VA converter has no sink pad template"))?;
        let formats = converter_dma_buf_formats(input.caps())?;
        dimensions
            .iter()
            .zip(supported)
            .map(|(&(width, height), supported)| {
                let mut accepted = Vec::new();
                if supported {
                    for (format, text) in &formats {
                        if accepts_dma_buf_format(input.caps(), text, width, height, cadence)?
                            && !accepted.contains(format)
                        {
                            accepted.push(*format);
                        }
                    }
                }
                Ok(accepted)
            })
            .collect()
    }

    pub(crate) fn validate_input(&self, layout: &VideoInputLayout) -> Result<(), MediaGraphError> {
        match (self, layout) {
            (Self::Software(_), VideoInputLayout::SystemMemoryBgrx) => Ok(()),
            (Self::Software(_), VideoInputLayout::DmaBuf { .. }) => Err(MediaGraphError::new(
                "the software video encoder requires system-memory BGRx input",
            )),
            (Self::VaH264 { .. }, VideoInputLayout::DmaBuf { .. }) => Ok(()),
            (Self::VaH264 { .. }, VideoInputLayout::SystemMemoryBgrx) => Err(MediaGraphError::new(
                "the VA H.264 encoder requires DMA-BUF input",
            )),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Software(VideoCodec::Vp8) => vp8::ENCODER_NAME,
            Self::Software(VideoCodec::H264) => h264::ENCODER_NAME,
            Self::VaH264 { .. } => "vah264enc",
        }
    }

    pub(crate) fn input_caps(&self, cadence: VideoCadence) -> Result<gst::Caps, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => vp8::encoder_input_caps(cadence),
            Self::Software(VideoCodec::H264) => h264::encoder_input_caps(cadence),
            Self::VaH264 { .. } => format!(
                "video/x-raw(memory:VAMemory),format=(string)NV12,framerate=(fraction){}",
                cadence.caps_fraction()
            )
            .parse::<gst::Caps>()
            .map_err(|error| {
                MediaGraphError::new(format!("construct VA encoder input caps: {error}"))
            }),
        }
    }

    pub(crate) fn build_converter(&self) -> Result<gst::Element, MediaGraphError> {
        match self {
            Self::Software(_) => gst::ElementFactory::make("videoconvert")
                .name("pronk-video-convert")
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct video converter: {error}"))
                }),
            Self::VaH264 { render_node } => {
                let factory = selected_va_factory(
                    "postproc",
                    render_node,
                    &[("disable-passthrough", VaPropertyValue::Boolean)],
                )?;
                let converter = gst::ElementFactory::make(&factory)
                    .name("pronk-va-video-convert")
                    .property("disable-passthrough", true)
                    .build()
                    .map_err(|error| {
                        MediaGraphError::new(format!(
                            "construct selected VA video converter {factory}: {error}"
                        ))
                    })?;
                validate_va_device(&converter, render_node)?;
                Ok(converter)
            }
        }
    }

    pub(crate) fn memory_path(&self) -> &'static str {
        match self {
            Self::Software(_) => "system-memory BGRx to system-memory I420",
            Self::VaH264 { .. } => "DMA-BUF DMA_DRM to VA-memory NV12",
        }
    }

    pub(crate) fn build(
        &self,
        bitrate: NonZeroU64,
        cadence: VideoCadence,
    ) -> Result<gst::Element, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => gst::ElementFactory::make(vp8::ENCODER_NAME)
                .name("pronk-vp8-encoder")
                .property("deadline", 1_i64)
                .property("cpu-used", 8_i32)
                .property_from_str("end-usage", "cbr")
                .property("undershoot", 100_i32)
                .property("overshoot", 15_i32)
                .property("buffer-initial-size", 500_i32)
                .property("buffer-optimal-size", 600_i32)
                .property("buffer-size", 1_000_i32)
                .property("target-bitrate", vp8::bitrate(bitrate.get())?)
                .property_from_str("keyframe-mode", "disabled")
                .property("keyframe-max-dist", vp8::key_frame_interval(cadence))
                .property("lag-in-frames", 0_i32)
                .property("threads", 8_i32)
                .property("static-threshold", 100_i32)
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct {}: {error}", vp8::ENCODER_NAME))
                }),
            Self::Software(VideoCodec::H264) => gst::ElementFactory::make(h264::ENCODER_NAME)
                .name("pronk-h264-encoder")
                .property_from_str("tune", "zerolatency")
                .property_from_str("speed-preset", "ultrafast")
                .property("bitrate", h264::bitrate_kbits(bitrate.get())?)
                .property("key-int-max", h264::key_frame_interval(cadence))
                .property("bframes", 0_u32)
                .property("byte-stream", true)
                .property("aud", true)
                .property("sliced-threads", true)
                .build()
                .map_err(|error| {
                    MediaGraphError::new(format!("construct {}: {error}", h264::ENCODER_NAME))
                }),
            Self::VaH264 { render_node } => {
                let key_frame_interval = h264::key_frame_interval(cadence);
                let bitrate = h264::bitrate_kbits(bitrate.get())?;
                let factory = selected_va_factory(
                    "h264enc",
                    render_node,
                    &[
                        ("bitrate", VaPropertyValue::Unsigned(bitrate)),
                        ("key-int-max", VaPropertyValue::Unsigned(key_frame_interval)),
                        ("b-frames", VaPropertyValue::Unsigned(0)),
                        ("cabac", VaPropertyValue::Boolean),
                        ("dct8x8", VaPropertyValue::Boolean),
                        ("aud", VaPropertyValue::Boolean),
                        ("rate-control", VaPropertyValue::Text("cbr")),
                    ],
                )?;
                let encoder = gst::ElementFactory::make(&factory)
                    .name("pronk-va-h264-encoder")
                    .property("bitrate", bitrate)
                    .property("key-int-max", key_frame_interval)
                    .property("b-frames", 0_u32)
                    .property("cabac", false)
                    .property("dct8x8", false)
                    .property("aud", true)
                    .property_from_str("rate-control", "cbr")
                    .build()
                    .map_err(|error| {
                        MediaGraphError::new(format!(
                            "construct selected VA H.264 encoder {factory}: {error}"
                        ))
                    })?;
                validate_va_device(&encoder, render_node)?;
                Ok(encoder)
            }
        }
    }

    pub(crate) fn effective_bitrate(&self, bitrate: NonZeroU64) -> Result<u64, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => u64::try_from(vp8::bitrate(bitrate.get())?)
                .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative")),
            Self::Software(VideoCodec::H264) => {
                Ok(u64::from(h264::bitrate_kbits(bitrate.get())?).saturating_mul(1_000))
            }
            Self::VaH264 { .. } => {
                Ok(u64::from(h264::bitrate_kbits(bitrate.get())?).saturating_mul(1_000))
            }
        }
    }

    pub(crate) fn set_bitrate(
        &self,
        encoder: &gst::Element,
        bitrate: NonZeroU64,
    ) -> Result<u64, MediaGraphError> {
        match self {
            Self::Software(VideoCodec::Vp8) => {
                let bitrate = vp8::bitrate(bitrate.get())?;
                encoder.set_property("target-bitrate", bitrate);
                u64::try_from(bitrate)
                    .map_err(|_| MediaGraphError::new("validated VP8 bitrate is negative"))
            }
            Self::Software(VideoCodec::H264) => {
                let bitrate = h264::bitrate_kbits(bitrate.get())?;
                encoder.set_property("bitrate", bitrate);
                Ok(u64::from(bitrate).saturating_mul(1_000))
            }
            Self::VaH264 { .. } => {
                let bitrate = h264::bitrate_kbits(bitrate.get())?;
                encoder.set_property("bitrate", bitrate);
                Ok(u64::from(bitrate).saturating_mul(1_000))
            }
        }
    }
}

fn supported_va_dimensions(
    compatible: &gst::CapsRef,
    dimensions: &[(u32, u32)],
    cadence: VideoCadence,
) -> Result<Vec<bool>, MediaGraphError> {
    dimensions
        .iter()
        .map(|&(width, height)| {
            let requested = format!(
                "video/x-raw(memory:VAMemory),format=(string)NV12,width=(int){width},height=(int){height},framerate=(fraction){}",
                cadence.caps_fraction()
            )
            .parse::<gst::Caps>()
            .map_err(|error| {
                MediaGraphError::new(format!("construct VA picture-size caps: {error}"))
            })?;
            Ok(compatible.can_intersect(&requested))
        })
        .collect()
}

fn supported_dma_buf_dimensions(
    input: &gst::CapsRef,
    dimensions: &[(u32, u32)],
    cadence: VideoCadence,
) -> Result<Vec<bool>, MediaGraphError> {
    dimensions
        .iter()
        .map(|&(width, height)| {
            let requested = format!(
                "video/x-raw(memory:DMABuf),format=(string)DMA_DRM,width=(int){width},height=(int){height},framerate=(fraction){}",
                cadence.caps_fraction()
            )
            .parse::<gst::Caps>()
            .map_err(|error| {
                MediaGraphError::new(format!("construct DMA-BUF picture-size caps: {error}"))
            })?;
            Ok(input.can_intersect(&requested))
        })
        .collect()
}

fn converter_dma_buf_formats(
    caps: &gst::CapsRef,
) -> Result<Vec<(DrmVideoFormat, String)>, MediaGraphError> {
    let mut formats = Vec::new();
    for (structure, features) in caps.iter_with_features() {
        // The PipeWire source supplies DMA-BUF memory without additional
        // caps features; a branch requiring metadata cannot negotiate it.
        if features.size() != 1
            || !features.contains("memory:DMABuf")
            || structure.get::<&str>("format").ok() != Some("DMA_DRM")
        {
            continue;
        }
        let mut add = |text: &str| -> Result<(), MediaGraphError> {
            let entry = (DrmVideoFormat::parse(text)?, text.to_owned());
            if !formats.contains(&entry) {
                formats.push(entry);
            }
            Ok(())
        };
        if let Ok(values) = structure.get::<gst::ListRef<'_>>("drm-format") {
            for value in values.iter() {
                let text = value.get::<&str>().map_err(|_| {
                    MediaGraphError::new("VA converter exposes a non-string DRM format")
                })?;
                add(text)?;
            }
        } else {
            let text = structure.get::<&str>("drm-format").map_err(|_| {
                MediaGraphError::new("VA converter exposes no concrete DRM formats")
            })?;
            add(text)?;
        }
    }
    if formats.is_empty() {
        return Err(MediaGraphError::new(
            "VA converter exposes no DMA-BUF DRM formats",
        ));
    }
    Ok(formats)
}

fn accepts_dma_buf_format(
    input: &gst::CapsRef,
    drm_format: &str,
    width: u32,
    height: u32,
    cadence: VideoCadence,
) -> Result<bool, MediaGraphError> {
    let integer = |value| {
        i32::try_from(value)
            .map_err(|_| MediaGraphError::new("VA picture dimension exceeds caps range"))
    };
    let requested = gst::Caps::builder("video/x-raw")
        .features(["memory:DMABuf"])
        .field("format", "DMA_DRM")
        .field("drm-format", drm_format)
        .field("width", integer(width)?)
        .field("height", integer(height)?)
        .field(
            "framerate",
            gst::Fraction::new(
                integer(cadence.numerator.get())?,
                integer(cadence.denominator.get())?,
            ),
        )
        .build();
    Ok(input.can_intersect(&requested))
}

fn require_va_baseline_output(encoder: &gst::Element) -> Result<(), MediaGraphError> {
    let output = encoder
        .pad_template("src")
        .ok_or_else(|| MediaGraphError::new("VA H.264 encoder has no source pad template"))?;
    if !supports_va_baseline_caps(output.caps())? {
        return Err(MediaGraphError::new(
            "selected VA H.264 encoder does not advertise constrained-baseline output",
        ));
    }
    Ok(())
}

fn supports_va_baseline_caps(output: &gst::CapsRef) -> Result<bool, MediaGraphError> {
    let baseline: gst::Caps = "video/x-h264,profile=(string)constrained-baseline"
        .parse()
        .map_err(|error| {
            MediaGraphError::new(format!("construct constrained-baseline caps: {error}"))
        })?;
    Ok(output.can_intersect(&baseline))
}

#[derive(Clone, Copy)]
enum VaPropertyValue<'a> {
    Boolean,
    Unsigned(u32),
    Text(&'a str),
}

fn selected_va_factory(
    suffix: &str,
    render_node: &Path,
    properties: &[(&str, VaPropertyValue<'_>)],
) -> Result<String, MediaGraphError> {
    let metadata = render_node.metadata().map_err(|error| {
        MediaGraphError::new(format!(
            "inspect selected render device {}: {error}",
            render_node.display()
        ))
    })?;
    if !metadata.file_type().is_char_device() {
        return Err(MediaGraphError::new(format!(
            "selected render device {} is not a character device",
            render_node.display()
        )));
    }
    gst::init().map_err(|error| {
        MediaGraphError::new(format!(
            "initialize GStreamer while selecting a VA device: {error}"
        ))
    })?;
    let mut factories: Vec<String> = gst::Registry::get()
        .features_by_plugin("va")
        .into_iter()
        .filter_map(|feature| feature.downcast::<gst::ElementFactory>().ok())
        .map(|factory| factory.name().to_string())
        .filter(|name| name.starts_with("va") && name.ends_with(suffix))
        .collect();
    let default_factory = format!("va{suffix}");
    factories.sort();
    factories.sort_by_key(|name| name != &default_factory);
    let mut available = Vec::new();
    for factory in factories {
        let Ok(element) = gst::ElementFactory::make(&factory).build() else {
            continue;
        };
        match validate_va_device(&element, render_node)
            .and_then(|()| validate_va_properties(&element, properties))
        {
            Ok(()) => return Ok(factory),
            Err(error) => available.push(format!("{factory}: {error}")),
        }
    }
    Err(MediaGraphError::new(format!(
        "the selected render device {} has no VA {suffix} element (available: {})",
        render_node.display(),
        available.join(", ")
    )))
}

fn validate_va_properties(
    element: &gst::Element,
    properties: &[(&str, VaPropertyValue<'_>)],
) -> Result<(), MediaGraphError> {
    for &(name, value) in properties {
        let property = element.find_property(name).ok_or_else(|| {
            MediaGraphError::new(format!("{} has no {name} property", element.name()))
        })?;
        if !property.flags().contains(gst::glib::ParamFlags::WRITABLE) {
            return Err(MediaGraphError::new(format!(
                "{} cannot set its {name} property",
                element.name(),
            )));
        }
        if !va_property_accepts(&property, value) {
            return Err(MediaGraphError::new(format!(
                "{} does not accept {name}",
                element.name()
            )));
        }
    }
    Ok(())
}

fn va_property_accepts(property: &gst::glib::ParamSpec, value: VaPropertyValue<'_>) -> bool {
    match value {
        VaPropertyValue::Boolean => property.is::<gst::glib::ParamSpecBoolean>(),
        VaPropertyValue::Unsigned(value) => property
            .downcast_ref::<gst::glib::ParamSpecUInt>()
            .is_some_and(|spec| (spec.minimum()..=spec.maximum()).contains(&value)),
        VaPropertyValue::Text(value) => {
            gst::glib::Value::deserialize_with_pspec(value, property).is_ok()
        }
    }
}

fn validate_va_device(element: &gst::Element, requested: &Path) -> Result<(), MediaGraphError> {
    let actual = va_device_path(element)?;
    let requested_metadata = requested.metadata().map_err(|error| {
        MediaGraphError::new(format!(
            "inspect selected render device {}: {error}",
            requested.display()
        ))
    })?;
    if !requested_metadata.file_type().is_char_device() {
        return Err(MediaGraphError::new(format!(
            "selected render device {} is not a character device",
            requested.display()
        )));
    }
    let actual_metadata = Path::new(&actual).metadata().map_err(|error| {
        MediaGraphError::new(format!(
            "inspect {actual} selected by {}: {error}",
            element.name()
        ))
    })?;
    if !actual_metadata.file_type().is_char_device()
        || requested_metadata.rdev() != actual_metadata.rdev()
    {
        return Err(MediaGraphError::new(format!(
            "{} selected {actual}, not {}",
            element.name(),
            requested.display()
        )));
    }
    Ok(())
}

fn va_device_path(element: &gst::Element) -> Result<String, MediaGraphError> {
    if element.find_property("device-path").is_none() {
        return Err(MediaGraphError::new(format!(
            "{} does not identify a VA render device",
            element.name()
        )));
    }
    element
        .property::<Option<String>>("device-path")
        .ok_or_else(|| {
            MediaGraphError::new(format!(
                "{} has no selected VA render device",
                element.name()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cadence() -> VideoCadence {
        VideoCadence::new(
            std::num::NonZeroU32::new(30).unwrap(),
            std::num::NonZeroU32::new(1).unwrap(),
        )
    }

    #[test]
    fn software_encoding_requires_its_system_memory_layout() {
        let encoder = VideoEncoder::software(VideoCodec::H264);
        assert!(encoder
            .validate_input(&VideoInputLayout::SystemMemoryBgrx)
            .is_ok());
        assert!(encoder
            .validate_input(&VideoInputLayout::DmaBuf {
                drm_format: DrmVideoFormat {
                    format: u32::from_le_bytes(*b"AR24"),
                    modifier: 0x0100_0000_0000_0009,
                },
            })
            .is_err());
    }

    #[test]
    fn va_h264_encoding_requires_dma_buf_input() {
        gst::init().unwrap();
        let encoder = VideoEncoder::va_h264("/dev/dri/renderD128");
        assert_eq!(encoder.codec(), VideoCodec::H264);
        assert!(encoder
            .validate_input(&VideoInputLayout::DmaBuf {
                drm_format: DrmVideoFormat {
                    format: u32::from_le_bytes(*b"AR24"),
                    modifier: 0x0100_0000_0000_0009,
                },
            })
            .is_ok());
        assert!(encoder
            .validate_input(&VideoInputLayout::SystemMemoryBgrx)
            .is_err());
        assert!(encoder
            .input_caps(VideoCadence::new(
                std::num::NonZeroU32::new(30_000).unwrap(),
                std::num::NonZeroU32::new(1_001).unwrap(),
            ))
            .unwrap()
            .to_string()
            .contains("memory:VAMemory"));
    }

    #[test]
    fn va_element_selection_rejects_a_regular_file_as_a_render_device() {
        let executable = std::env::current_exe().unwrap();
        let error = selected_va_factory("h264enc", &executable, &[]).unwrap_err();
        assert!(error.to_string().contains("not a character device"));
    }

    #[test]
    fn va_property_preflight_reports_unsupported_encoder_controls() {
        gst::init().unwrap();
        let element = gst::ElementFactory::make("fakesink").build().unwrap();
        let missing = validate_va_properties(
            &element,
            &[("rate-control", VaPropertyValue::Text("cbr"))],
        )
        .unwrap_err();
        assert!(missing.to_string().contains("rate-control property"));
        let readonly = validate_va_properties(
            &element,
            &[("last-sample", VaPropertyValue::Boolean)],
        )
        .unwrap_err();
        assert!(readonly.to_string().contains("cannot set its last-sample property"));
        let unsupported = validate_va_properties(
            &element,
            &[("state-error", VaPropertyValue::Text("cbr"))],
        )
        .unwrap_err();
        assert!(unsupported.to_string().contains("does not accept state-error"));
        let wrong_type = validate_va_properties(
            &element,
            &[("num-buffers", VaPropertyValue::Unsigned(0))],
        )
        .unwrap_err();
        assert!(wrong_type.to_string().contains("does not accept num-buffers"));
        assert!(validate_va_properties(
            &element,
            &[("enable-last-sample", VaPropertyValue::Boolean)],
        )
        .is_ok());
    }

    #[test]
    fn va_property_preflight_respects_unsigned_limits() {
        let property = gst::glib::ParamSpecUInt::builder("bitrate")
            .minimum(1)
            .maximum(100)
            .default_value(1)
            .build();
        assert!(!va_property_accepts(&property, VaPropertyValue::Unsigned(0)));
        assert!(va_property_accepts(&property, VaPropertyValue::Unsigned(100)));
        assert!(!va_property_accepts(&property, VaPropertyValue::Unsigned(101)));
    }

    #[test]
    fn va_device_validation_rejects_an_element_without_a_device_property() {
        gst::init().unwrap();
        let converter = gst::ElementFactory::make("videoconvert").build().unwrap();
        assert!(validate_va_device(&converter, Path::new("/dev/null")).is_err());
    }

    #[test]
    fn va_size_probe_respects_the_encoder_and_converter_intersection() {
        gst::init().unwrap();
        let converter: gst::Caps = "video/x-raw(memory:VAMemory),format=(string)NV12,width=(int)[32,4096],height=(int)[32,4096]"
            .parse()
            .unwrap();
        let encoder: gst::Caps = "video/x-raw(memory:VAMemory),format=(string)NV12,width=(int)[64,1920],height=(int)[64,1080]"
            .parse()
            .unwrap();
        assert_eq!(
            supported_va_dimensions(
                &converter.intersect(&encoder),
                &[(1920, 1080), (2560, 1440), (320, 240)],
                cadence(),
            )
            .unwrap(),
            [true, false, true]
        );
    }

    #[test]
    fn va_size_probe_does_not_combine_disjoint_size_ranges() {
        gst::init().unwrap();
        let compatible: gst::Caps = concat!(
            "video/x-raw(memory:VAMemory),format=(string)NV12,width=(int)[32,1920],height=(int)[32,1080];",
            "video/x-raw(memory:VAMemory),format=(string)NV12,width=(int)[2560,4096],height=(int)[32,720]"
        )
        .parse()
        .unwrap();
        assert_eq!(
            supported_va_dimensions(
                &compatible,
                &[(1920, 1080), (3840, 720), (3840, 2160)],
                cadence(),
            )
            .unwrap(),
            [true, true, false]
        );
    }

    #[test]
    fn va_size_probe_uses_the_requested_encoder_cadence() {
        gst::init().unwrap();
        let compatible: gst::Caps = "video/x-raw(memory:VAMemory),format=(string)NV12,width=(int)1920,height=(int)1080,framerate=(fraction)[1/1,30/1]"
            .parse()
            .unwrap();
        let sixty = VideoCadence::new(
            std::num::NonZeroU32::new(60).unwrap(),
            std::num::NonZeroU32::new(1).unwrap(),
        );
        assert_eq!(
            supported_va_dimensions(&compatible, &[(1920, 1080)], cadence()).unwrap(),
            [true]
        );
        assert_eq!(
            supported_va_dimensions(&compatible, &[(1920, 1080)], sixty).unwrap(),
            [false]
        );
    }

    #[test]
    fn va_size_probe_also_checks_dma_buf_input_limits() {
        gst::init().unwrap();
        let input: gst::Caps = "video/x-raw(memory:DMABuf),format=(string)DMA_DRM,drm-format=(string)AR24:0x0100000000000009,width=(int)[1,1920],height=(int)[1,1080],framerate=(fraction)[1/1,60/1]"
            .parse()
            .unwrap();
        let dimensions = [(1920, 1080), (2560, 1440)];
        assert_eq!(
            supported_dma_buf_dimensions(&input, &dimensions, cadence()).unwrap(),
            [true, false]
        );
    }

    #[test]
    fn va_format_probe_keeps_input_limits_with_each_format() {
        gst::init().unwrap();
        let input: gst::Caps = "video/x-raw(memory:DMABuf),format=(string)DMA_DRM,drm-format=(string)AR24:0x0000000000000009,width=(int)[1,1920],height=(int)[1,1080],framerate=(fraction)[1/1,60/1]; video/x-raw(memory:DMABuf),format=(string)DMA_DRM,drm-format=(string)AB24:0x0000000000000009,width=(int)[1,3840],height=(int)[1,2160],framerate=(fraction)[1/1,60/1]"
            .parse()
            .unwrap();
        let formats = converter_dma_buf_formats(&input).unwrap();
        assert_eq!(formats.len(), 2);
        assert!(accepts_dma_buf_format(&input, &formats[0].1, 1920, 1080, cadence()).unwrap());
        assert!(!accepts_dma_buf_format(&input, &formats[0].1, 3840, 2160, cadence()).unwrap());
        assert!(accepts_dma_buf_format(&input, &formats[1].1, 3840, 2160, cadence()).unwrap());
    }

    #[test]
    fn additional_caps_features_are_not_supplied_by_the_pipewire_source() {
        gst::init().unwrap();
        let requested: gst::Caps = "video/x-raw(memory:DMABuf),format=(string)DMA_DRM,drm-format=(string)AR24:0x0100000000000009"
            .parse()
            .unwrap();
        let requires_metadata: gst::Caps = "video/x-raw(memory:DMABuf,meta:Extra),format=(string)DMA_DRM,drm-format=(string)AR24:0x0100000000000009"
            .parse()
            .unwrap();
        assert!(!requested.can_intersect(&requires_metadata));
    }

    #[test]
    fn va_probe_requires_the_receiver_compatible_h264_profile() {
        gst::init().unwrap();
        let baseline: gst::Caps = "video/x-h264,profile=(string)constrained-baseline"
            .parse()
            .unwrap();
        let main: gst::Caps = "video/x-h264,profile=(string)main".parse().unwrap();
        assert!(supports_va_baseline_caps(&baseline).unwrap());
        assert!(!supports_va_baseline_caps(&main).unwrap());
    }

    #[test]
    #[ignore = "requires PRONK_GPU_RENDER_NODE and a matching VA converter"]
    fn selected_va_converter_reports_concrete_dma_buf_formats() {
        let render_node = std::env::var_os("PRONK_GPU_RENDER_NODE")
            .expect("PRONK_GPU_RENDER_NODE names the selected VA render node");
        let encoder = VideoEncoder::va_h264(render_node);
        let formats = encoder.supported_dma_buf_formats(cadence()).unwrap();
        assert!(!formats.is_empty());
        assert!(formats.iter().all(|format| format.format != 0));
        let dimensions = encoder
            .supported_dimensions(&[(1920, 1080), (2560, 1440), (3840, 2160)], cadence())
            .unwrap();
        eprintln!("selected VA encoder supports offered sizes: {dimensions:?}");
        assert!(dimensions[0]);
        let by_size = encoder
            .supported_dma_buf_formats_for_dimensions(
                &[(1920, 1080), (2560, 1440), (3840, 2160)],
                cadence(),
            )
            .unwrap();
        eprintln!("selected VA converter accepts formats by size: {by_size:?}");
        assert!(!by_size[0].is_empty());
        assert!(by_size
            .iter()
            .flatten()
            .all(|format| formats.contains(format)));
    }
}
