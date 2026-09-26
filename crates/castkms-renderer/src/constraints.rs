//! Owned whole-scene constraints declared by a renderer endpoint.

use std::io;
use std::num::NonZeroU32;

use castkms_sys::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888,
    RENDERER_CONSTRAINTS_FORMAT_BYTES, RENDERER_CONSTRAINTS_FORMAT_EXPLICIT_MODIFIER,
    RENDERER_CONSTRAINTS_FORMAT_IMPORTED, RENDERER_CONSTRAINTS_FORMAT_NATIVE,
    RENDERER_CONSTRAINTS_HEADER_BYTES, RENDERER_CONSTRAINTS_KIND, RENDERER_CONSTRAINTS_MAX_FORMATS,
    RENDERER_CONSTRAINTS_OUTPUT_MATRIX, RENDERER_CONSTRAINTS_ROLE_PRIMARY,
    RENDERER_CONSTRAINTS_VERSION,
};
use drm_display_executor::scene::geometry::Extent;

use crate::FormatModifier;

const FIXED_SCALE: u32 = 1 << 16;

/// Storage provenance accepted for one exact format tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageProvenance {
    Native,
    Imported,
    Both,
}

impl StorageProvenance {
    pub const fn native(self) -> bool {
        matches!(self, Self::Native | Self::Both)
    }

    pub const fn imported(self) -> bool {
        matches!(self, Self::Imported | Self::Both)
    }
}

/// One exact format, modifier and memory-plane tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConstraintsFormat {
    fourcc: u32,
    modifier: FormatModifier,
    memory_plane_count: NonZeroU32,
    provenance: StorageProvenance,
    roles: u32,
    width_alignment: NonZeroU32,
    height_alignment: NonZeroU32,
    pitch_alignment: NonZeroU32,
    offset_alignment: NonZeroU32,
    min_pitch: NonZeroU32,
    max_pitch: NonZeroU32,
}

impl ConstraintsFormat {
    pub fn new(
        fourcc: u32,
        modifier: FormatModifier,
        memory_plane_count: NonZeroU32,
        provenance: StorageProvenance,
        pitch_alignment: NonZeroU32,
        offset_alignment: NonZeroU32,
        max_pitch: NonZeroU32,
    ) -> io::Result<Self> {
        if fourcc == 0
            || memory_plane_count.get() > 4
            || !pitch_alignment.is_power_of_two()
            || !offset_alignment.is_power_of_two()
            || max_pitch < pitch_alignment
            || modifier == FormatModifier::Explicit(DRM_FORMAT_MOD_INVALID)
        {
            return Err(invalid("invalid renderer storage constraints"));
        }
        Ok(Self {
            fourcc,
            modifier,
            memory_plane_count,
            provenance,
            roles: RENDERER_CONSTRAINTS_ROLE_PRIMARY,
            width_alignment: NonZeroU32::MIN,
            height_alignment: NonZeroU32::MIN,
            pitch_alignment,
            offset_alignment,
            min_pitch: pitch_alignment,
            max_pitch,
        })
    }

    pub fn fourcc(self) -> u32 {
        self.fourcc
    }

    pub fn modifier(self) -> FormatModifier {
        self.modifier
    }

    pub fn memory_plane_count(self) -> NonZeroU32 {
        self.memory_plane_count
    }

    pub fn provenance(self) -> StorageProvenance {
        self.provenance
    }

    pub fn pitch_alignment(self) -> NonZeroU32 {
        self.pitch_alignment
    }

    pub fn offset_alignment(self) -> NonZeroU32 {
        self.offset_alignment
    }

    pub fn max_pitch(self) -> NonZeroU32 {
        self.max_pitch
    }
}

/// Immutable whole-scene constraints declared by a delegated renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererConstraints {
    flags: u32,
    min_output: Extent,
    max_output: Extent,
    min_source: Extent,
    max_source: Extent,
    min_scale: NonZeroU32,
    max_scale: NonZeroU32,
    max_planes: NonZeroU32,
    max_roles: [u32; 3],
    max_color_operations: u32,
    max_lut_entries: u32,
    yuv_encodings: u32,
    yuv_ranges: u32,
    formats: Box<[ConstraintsFormat]>,
}

impl RendererConstraints {
    /// Describe the exact linear XRGB8888 contract used by the reference worker.
    pub fn linear_xrgb8888_primary(output: Extent) -> Self {
        let format = ConstraintsFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Explicit(DRM_FORMAT_MOD_LINEAR),
            NonZeroU32::new(1).expect("one memory plane is nonzero"),
            StorageProvenance::Both,
            NonZeroU32::new(1).expect("unit pitch alignment is nonzero"),
            NonZeroU32::new(1).expect("unit offset alignment is nonzero"),
            NonZeroU32::new(u32::MAX).expect("maximum pitch is nonzero"),
        )
        .expect("linear XRGB8888 is valid renderer storage");
        Self::single_primary(output, format)
    }

    /// Limit a backend to one unscaled full-output primary plane.
    pub fn single_primary(output: Extent, format: ConstraintsFormat) -> Self {
        Self::single_primary_formats(output, vec![format].into_boxed_slice())
            .expect("one valid format is valid primary constraints")
    }

    /// Limit a backend to one full-output primary plane with alternatives.
    pub fn single_primary_formats(
        output: Extent,
        formats: Box<[ConstraintsFormat]>,
    ) -> io::Result<Self> {
        if formats.is_empty()
            || formats.len() > RENDERER_CONSTRAINTS_MAX_FORMATS
            || formats.iter().enumerate().any(|(index, format)| {
                formats[..index].iter().any(|previous| {
                    previous.fourcc == format.fourcc
                        && previous.modifier == format.modifier
                        && previous.memory_plane_count == format.memory_plane_count
                })
            })
        {
            return Err(invalid("invalid primary format set"));
        }
        Ok(Self {
            flags: 0,
            min_output: output,
            max_output: output,
            min_source: output,
            max_source: output,
            min_scale: NonZeroU32::new(FIXED_SCALE).expect("fixed scale is nonzero"),
            max_scale: NonZeroU32::new(FIXED_SCALE).expect("fixed scale is nonzero"),
            max_planes: NonZeroU32::new(1).expect("one plane is nonzero"),
            max_roles: [1, 0, 0],
            max_color_operations: 0,
            max_lut_entries: 0,
            yuv_encodings: 0,
            yuv_ranges: 0,
            formats,
        })
    }

    /// Accept the standard output color pipeline after scene composition.
    pub fn with_output_color(mut self, max_lut_entries: u32, matrix: bool) -> io::Result<Self> {
        if max_lut_entries > 256 {
            return Err(invalid("invalid output color constraints"));
        }
        if matrix {
            self.flags |= RENDERER_CONSTRAINTS_OUTPUT_MATRIX;
        } else {
            self.flags &= !RENDERER_CONSTRAINTS_OUTPUT_MATRIX;
        }
        self.max_color_operations = u32::from(max_lut_entries > 0) * 2 + u32::from(matrix);
        self.max_lut_entries = max_lut_entries;
        Ok(self)
    }

    pub fn max_output(&self) -> Extent {
        self.max_output
    }

    pub fn min_output(&self) -> Extent {
        self.min_output
    }

    pub fn max_source(&self) -> Extent {
        self.max_source
    }

    pub fn min_source(&self) -> Extent {
        self.min_source
    }

    pub fn formats(&self) -> &[ConstraintsFormat] {
        &self.formats
    }

    pub(crate) fn contains_output(&self, output: Extent) -> bool {
        output.width() >= self.min_output.width()
            && output.height() >= self.min_output.height()
            && output.width() <= self.max_output.width()
            && output.height() <= self.max_output.height()
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            RENDERER_CONSTRAINTS_HEADER_BYTES
                + self.formats.len() * RENDERER_CONSTRAINTS_FORMAT_BYTES,
        );
        for value in [
            RENDERER_CONSTRAINTS_VERSION,
            RENDERER_CONSTRAINTS_KIND,
            self.flags,
            self.formats.len() as u32,
            self.max_output.width(),
            self.max_output.height(),
            self.max_source.width(),
            self.max_source.height(),
            self.min_scale.get(),
            self.max_scale.get(),
            self.max_planes.get(),
            self.max_roles[0],
            self.max_roles[1],
            self.max_roles[2],
            self.max_color_operations,
            self.max_lut_entries,
            self.yuv_encodings,
            self.yuv_ranges,
            self.min_output.width(),
            self.min_output.height(),
            self.min_source.width(),
            self.min_source.height(),
        ] {
            put_u32(&mut bytes, value);
        }
        bytes.resize(RENDERER_CONSTRAINTS_HEADER_BYTES, 0);
        for format in &self.formats {
            put_format(&mut bytes, *format);
        }
        bytes
    }
}

fn put_format(bytes: &mut Vec<u8>, format: ConstraintsFormat) {
    put_u32(bytes, format.fourcc);
    put_u32(bytes, format.memory_plane_count.get());
    let (modifier, explicit) = match format.modifier {
        FormatModifier::Unspecified => (0, false),
        FormatModifier::Explicit(modifier) => (modifier, true),
    };
    put_u64(bytes, modifier);
    let flags = (u32::from(format.provenance.native()) * RENDERER_CONSTRAINTS_FORMAT_NATIVE)
        | (u32::from(format.provenance.imported()) * RENDERER_CONSTRAINTS_FORMAT_IMPORTED)
        | (u32::from(explicit) * RENDERER_CONSTRAINTS_FORMAT_EXPLICIT_MODIFIER);
    put_u32(bytes, flags);
    put_u32(bytes, format.roles);
    put_u32(bytes, format.width_alignment.get());
    put_u32(bytes, format.height_alignment.get());
    put_u32(bytes, format.pitch_alignment.get());
    put_u32(bytes, format.offset_alignment.get());
    put_u32(bytes, format.min_pitch.get());
    put_u32(bytes, format.max_pitch.get());
    put_u32(bytes, 0);
    put_u32(bytes, 0);
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format() -> ConstraintsFormat {
        ConstraintsFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Explicit(DRM_FORMAT_MOD_LINEAR),
            NonZeroU32::new(1).unwrap(),
            StorageProvenance::Both,
            NonZeroU32::new(4).unwrap(),
            NonZeroU32::new(4).unwrap(),
            NonZeroU32::new(65_536).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn narrow_primary_constraints_have_the_wire_layout() {
        let output = Extent::new(1920, 1080).unwrap();
        let constraints = RendererConstraints::single_primary(output, format());
        let bytes = constraints.encode();
        assert_eq!(
            bytes.len(),
            RENDERER_CONSTRAINTS_HEADER_BYTES + RENDERER_CONSTRAINTS_FORMAT_BYTES
        );
        assert_eq!(
            u32::from_ne_bytes(bytes[0..4].try_into().unwrap()),
            RENDERER_CONSTRAINTS_VERSION
        );
        assert_eq!(
            u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            RENDERER_CONSTRAINTS_KIND
        );
        assert_eq!(u32::from_ne_bytes(bytes[12..16].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(bytes[72..76].try_into().unwrap()), 1920);
        assert_eq!(u32::from_ne_bytes(bytes[76..80].try_into().unwrap()), 1080);
        assert!(bytes[88..RENDERER_CONSTRAINTS_HEADER_BYTES]
            .iter()
            .all(|byte| *byte == 0));
        let format = &bytes[RENDERER_CONSTRAINTS_HEADER_BYTES..];
        assert_eq!(u32::from_ne_bytes(format[20..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(format[24..28].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(format[28..32].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(format[32..36].try_into().unwrap()), 4);
        assert_eq!(u32::from_ne_bytes(format[40..44].try_into().unwrap()), 4);
        assert!(format[48..56].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn storage_provenance_encodes_each_supported_origin() {
        let output = Extent::new(1920, 1080).unwrap();
        for (provenance, expected_flags) in [
            (
                StorageProvenance::Native,
                RENDERER_CONSTRAINTS_FORMAT_NATIVE,
            ),
            (
                StorageProvenance::Imported,
                RENDERER_CONSTRAINTS_FORMAT_IMPORTED,
            ),
            (
                StorageProvenance::Both,
                RENDERER_CONSTRAINTS_FORMAT_NATIVE | RENDERER_CONSTRAINTS_FORMAT_IMPORTED,
            ),
        ] {
            let mut format = format();
            format.provenance = provenance;
            let bytes = RendererConstraints::single_primary(output, format).encode();
            let offset = RENDERER_CONSTRAINTS_HEADER_BYTES + 16;
            let flags = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            assert_eq!(
                flags & (RENDERER_CONSTRAINTS_FORMAT_NATIVE | RENDERER_CONSTRAINTS_FORMAT_IMPORTED),
                expected_flags
            );
        }
    }

    #[test]
    fn output_color_constraints_are_bounded() {
        let constraints =
            RendererConstraints::single_primary(Extent::new(1920, 1080).unwrap(), format())
                .with_output_color(256, true)
                .unwrap();
        let bytes = constraints.encode();
        assert_eq!(u32::from_ne_bytes(bytes[56..60].try_into().unwrap()), 3);
        assert_eq!(u32::from_ne_bytes(bytes[60..64].try_into().unwrap()), 256);
        assert!(
            RendererConstraints::single_primary(Extent::new(1920, 1080).unwrap(), format(),)
                .with_output_color(257, false)
                .is_err()
        );

        let matrix =
            RendererConstraints::single_primary(Extent::new(1920, 1080).unwrap(), format())
                .with_output_color(0, true)
                .unwrap()
                .encode();
        assert_eq!(u32::from_ne_bytes(matrix[56..60].try_into().unwrap()), 1);

        let luts = RendererConstraints::single_primary(Extent::new(1920, 1080).unwrap(), format())
            .with_output_color(1, false)
            .unwrap()
            .encode();
        assert_eq!(u32::from_ne_bytes(luts[56..60].try_into().unwrap()), 2);
    }

    #[test]
    fn storage_constraints_reject_invalid_modifier() {
        assert!(ConstraintsFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Explicit(DRM_FORMAT_MOD_INVALID),
            NonZeroU32::new(1).unwrap(),
            StorageProvenance::Native,
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
        )
        .is_err());
    }

    #[test]
    fn primary_constraints_require_unique_storage_alternatives() {
        let output = Extent::new(1920, 1080).unwrap();
        let mut duplicate = format();
        duplicate.provenance = StorageProvenance::Native;
        duplicate.pitch_alignment = NonZeroU32::new(8).unwrap();
        assert!(RendererConstraints::single_primary_formats(output, Box::new([])).is_err());
        assert!(RendererConstraints::single_primary_formats(
            output,
            vec![format(), duplicate].into_boxed_slice(),
        )
        .is_err());
    }

    #[test]
    fn target_output_must_be_inside_the_declared_range() {
        let constraints =
            RendererConstraints::single_primary(Extent::new(1920, 1080).unwrap(), format());
        assert!(constraints.contains_output(Extent::new(1920, 1080).unwrap()));
        assert!(!constraints.contains_output(Extent::new(1280, 720).unwrap()));
    }
}
