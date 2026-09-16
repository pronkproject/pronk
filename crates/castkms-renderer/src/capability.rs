//! Owned whole-scene contracts used during renderer transitions.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, BorrowedFd};

use castkms_sys::{
    drm_ioctl_castkms_renderer_query_capabilities, DrmCastkmsRendererQueryCapabilities,
    CAPABILITY_FORMAT_EXPLICIT_MODIFIER, CAPABILITY_FORMAT_IMPORTED, CAPABILITY_FORMAT_NATIVE,
    CAPABILITY_KIND_HOST, CAPABILITY_KIND_RENDERER, CAPABILITY_MAX_BYTES, CAPABILITY_MAX_FORMATS,
    CAPABILITY_PROFILE_CROP, CAPABILITY_PROFILE_FRACTIONAL, CAPABILITY_PROFILE_OUTPUT_MATRIX,
    CAPABILITY_PROFILE_PLANE_MATRIX, CAPABILITY_PROFILE_POSITION, CAPABILITY_PROFILE_SCALE,
    CAPABILITY_PROFILE_SRGB, CAPABILITY_QUERY_MAX_BYTES, CAPABILITY_STATE_GATED,
    CAPABILITY_STATE_PENDING, CAPABILITY_VERSION, CAPABILITY_YUV_ENCODING_BT2020,
    CAPABILITY_YUV_ENCODING_BT601, CAPABILITY_YUV_ENCODING_BT709, CAPABILITY_YUV_RANGE_FULL,
    CAPABILITY_YUV_RANGE_LIMITED, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR,
    DRM_FORMAT_XRGB8888,
};
use drm_display_executor::scene::geometry::Extent;

use crate::FormatModifier;

const PROFILE_BYTES: usize = 128;
const FORMAT_BYTES: usize = 32;
const FEATURE_FLAGS: u32 = CAPABILITY_PROFILE_CROP
    | CAPABILITY_PROFILE_FRACTIONAL
    | CAPABILITY_PROFILE_POSITION
    | CAPABILITY_PROFILE_SCALE
    | CAPABILITY_PROFILE_SRGB
    | CAPABILITY_PROFILE_PLANE_MATRIX
    | CAPABILITY_PROFILE_OUTPUT_MATRIX;
const YUV_ENCODINGS: u32 =
    CAPABILITY_YUV_ENCODING_BT601 | CAPABILITY_YUV_ENCODING_BT709 | CAPABILITY_YUV_ENCODING_BT2020;
const YUV_RANGES: u32 = CAPABILITY_YUV_RANGE_LIMITED | CAPABILITY_YUV_RANGE_FULL;
const SNAPSHOT_BYTES: usize = 72;
const FIXED_SCALE: u32 = 1 << 16;

/// Storage provenance accepted for one exact format tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageProvenance {
    native: bool,
    imported: bool,
}

impl StorageProvenance {
    pub const fn new(native: bool, imported: bool) -> Self {
        Self { native, imported }
    }

    pub const fn native(self) -> bool {
        self.native
    }

    pub const fn imported(self) -> bool {
        self.imported
    }
}

/// One exact format, modifier and memory-plane tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityFormat {
    fourcc: u32,
    modifier: FormatModifier,
    plane_count: NonZeroU32,
    provenance: StorageProvenance,
    pitch_alignment: NonZeroU32,
    offset_alignment: NonZeroU32,
    max_pitch: NonZeroU32,
}

impl CapabilityFormat {
    pub fn new(
        fourcc: u32,
        modifier: FormatModifier,
        plane_count: NonZeroU32,
        provenance: StorageProvenance,
        pitch_alignment: NonZeroU32,
        offset_alignment: NonZeroU32,
        max_pitch: NonZeroU32,
    ) -> io::Result<Self> {
        if fourcc == 0
            || plane_count.get() > 4
            || (!provenance.native && !provenance.imported)
            || !pitch_alignment.get().is_power_of_two()
            || !offset_alignment.get().is_power_of_two()
            || max_pitch < pitch_alignment
            || modifier == FormatModifier::Explicit(DRM_FORMAT_MOD_INVALID)
        {
            return Err(invalid("invalid renderer storage capability"));
        }
        Ok(Self {
            fourcc,
            modifier,
            plane_count,
            provenance,
            pitch_alignment,
            offset_alignment,
            max_pitch,
        })
    }

    pub fn fourcc(self) -> u32 {
        self.fourcc
    }

    pub fn modifier(self) -> FormatModifier {
        self.modifier
    }

    pub fn plane_count(self) -> NonZeroU32 {
        self.plane_count
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

/// Immutable whole-scene contract for a delegated renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererCapability {
    flags: u32,
    min_output: Extent,
    max_output: Extent,
    min_source: Extent,
    max_source: Extent,
    min_scale: NonZeroU32,
    max_scale: NonZeroU32,
    max_layers: NonZeroU32,
    max_roles: [u32; 3],
    max_color_operations: u32,
    max_lut_entries: u32,
    yuv_encodings: u32,
    yuv_ranges: u32,
    formats: Box<[CapabilityFormat]>,
}

impl RendererCapability {
    /// Describe the exact linear XRGB8888 contract used by the reference worker.
    pub fn linear_xrgb8888_primary(output: Extent) -> Self {
        let format = CapabilityFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Explicit(DRM_FORMAT_MOD_LINEAR),
            NonZeroU32::new(1).expect("one memory plane is nonzero"),
            StorageProvenance::new(true, true),
            NonZeroU32::new(1).expect("unit pitch alignment is nonzero"),
            NonZeroU32::new(1).expect("unit offset alignment is nonzero"),
            NonZeroU32::new(u32::MAX).expect("maximum pitch is nonzero"),
        )
        .expect("linear XRGB8888 is a valid capability format");
        Self::single_primary(output, format)
    }

    /// Limit a transition to one unscaled full-output primary plane.
    ///
    /// The narrow shape lets a renderer reserve complete private storage before
    /// it claims the first source-bearing job.
    pub fn single_primary(output: Extent, format: CapabilityFormat) -> Self {
        Self {
            flags: 0,
            min_output: output,
            max_output: output,
            min_source: output,
            max_source: output,
            min_scale: NonZeroU32::new(FIXED_SCALE).expect("fixed scale is nonzero"),
            max_scale: NonZeroU32::new(FIXED_SCALE).expect("fixed scale is nonzero"),
            max_layers: NonZeroU32::new(1).expect("one layer is nonzero"),
            max_roles: [1, 0, 0],
            max_color_operations: 0,
            max_lut_entries: 0,
            yuv_encodings: 0,
            yuv_ranges: 0,
            formats: vec![format].into_boxed_slice(),
        }
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

    pub fn formats(&self) -> &[CapabilityFormat] {
        &self.formats
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PROFILE_BYTES + self.formats.len() * FORMAT_BYTES);
        for value in [
            CAPABILITY_VERSION,
            CAPABILITY_KIND_RENDERER,
            self.flags,
            self.formats.len() as u32,
            self.max_output.width(),
            self.max_output.height(),
            self.max_source.width(),
            self.max_source.height(),
            self.min_scale.get(),
            self.max_scale.get(),
            self.max_layers.get(),
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
        bytes.resize(PROFILE_BYTES, 0);
        for format in &self.formats {
            put_format(&mut bytes, *format);
        }
        bytes
    }
}

/// Active or proposed validation contract returned by CastKMS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityProfile {
    Host,
    Renderer(RendererCapability),
}

/// One pending immutable contract and its compositor transition name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCapability {
    generation: NonZeroU64,
    transition: NonZeroU64,
    gated: bool,
    profile: CapabilityProfile,
}

impl PendingCapability {
    pub fn generation(&self) -> NonZeroU64 {
        self.generation
    }

    pub fn transition(&self) -> NonZeroU64 {
        self.transition
    }

    pub fn gated(&self) -> bool {
        self.gated
    }

    pub fn profile(&self) -> &CapabilityProfile {
        &self.profile
    }
}

/// Coherent execution and validation state for one renderer endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    execution_profile: crate::Profile,
    execution_generation: NonZeroU64,
    active_generation: NonZeroU64,
    validation_epoch: NonZeroU64,
    active: CapabilityProfile,
    pending: Option<PendingCapability>,
}

impl CapabilitySnapshot {
    pub fn execution_profile(&self) -> crate::Profile {
        self.execution_profile
    }

    pub fn execution_generation(&self) -> NonZeroU64 {
        self.execution_generation
    }

    pub fn active_generation(&self) -> NonZeroU64 {
        self.active_generation
    }

    pub fn validation_epoch(&self) -> NonZeroU64 {
        self.validation_epoch
    }

    pub fn active(&self) -> &CapabilityProfile {
        &self.active
    }

    pub fn pending(&self) -> Option<&PendingCapability> {
        self.pending.as_ref()
    }
}

pub(super) fn query(fd: BorrowedFd<'_>) -> io::Result<CapabilitySnapshot> {
    let mut bytes = vec![0_u8; CAPABILITY_QUERY_MAX_BYTES];
    let request = DrmCastkmsRendererQueryCapabilities {
        result: bytes.as_mut_ptr() as u64,
        capacity: bytes.len() as u32,
        ..Default::default()
    };
    // SAFETY: The initialized request and writable bounded byte buffer remain
    // live and stable throughout the synchronous ioctl.
    unsafe { drm_ioctl_castkms_renderer_query_capabilities(fd.as_raw_fd(), &request) }?;
    decode_snapshot(&bytes)
}

fn decode_snapshot(bytes: &[u8]) -> io::Result<CapabilitySnapshot> {
    if bytes.len() < SNAPSHOT_BYTES {
        return Err(invalid("truncated CastKMS capability snapshot"));
    }
    let version = get_u32(bytes, 0)?;
    let size = get_u32(bytes, 4)? as usize;
    let flags = get_u32(bytes, 12)?;
    if version != CAPABILITY_VERSION
        || !(SNAPSHOT_BYTES..=CAPABILITY_QUERY_MAX_BYTES).contains(&size)
        || size > bytes.len()
        || flags & !(CAPABILITY_STATE_PENDING | CAPABILITY_STATE_GATED) != 0
    {
        return Err(invalid("CastKMS returned an invalid capability snapshot"));
    }
    let active_offset = get_u32(bytes, 56)? as usize;
    let active_size = get_u32(bytes, 60)? as usize;
    let pending_offset = get_u32(bytes, 64)? as usize;
    let pending_size = get_u32(bytes, 68)? as usize;
    let active_end = active_offset
        .checked_add(active_size)
        .ok_or_else(|| invalid("CastKMS active capability range overflowed"))?;
    if active_offset != SNAPSHOT_BYTES || active_end > size {
        return Err(invalid(
            "CastKMS returned an invalid active capability range",
        ));
    }
    let active = CapabilityProfile::decode(&bytes[active_offset..active_end])?;
    let pending = if flags & CAPABILITY_STATE_PENDING != 0 {
        let pending_end = pending_offset
            .checked_add(pending_size)
            .ok_or_else(|| invalid("CastKMS pending capability range overflowed"))?;
        if pending_offset != active_end || pending_end != size {
            return Err(invalid(
                "CastKMS returned an invalid pending capability range",
            ));
        }
        Some(PendingCapability {
            generation: nonzero64(get_u64(bytes, 32)?, "zero pending capability generation")?,
            transition: nonzero64(get_u64(bytes, 40)?, "zero capability transition")?,
            gated: flags & CAPABILITY_STATE_GATED != 0,
            profile: CapabilityProfile::decode(&bytes[pending_offset..pending_end])?,
        })
    } else {
        if flags & CAPABILITY_STATE_GATED != 0
            || get_u64(bytes, 32)? != 0
            || get_u64(bytes, 40)? != 0
            || pending_offset != 0
            || pending_size != 0
            || active_end != size
        {
            return Err(invalid("CastKMS returned pending state without a profile"));
        }
        None
    };
    Ok(CapabilitySnapshot {
        execution_profile: crate::Profile::from_uapi(get_u32(bytes, 8)?)?,
        execution_generation: nonzero64(get_u64(bytes, 16)?, "zero execution generation")?,
        active_generation: nonzero64(get_u64(bytes, 24)?, "zero active capability generation")?,
        validation_epoch: nonzero64(get_u64(bytes, 48)?, "zero validation epoch")?,
        active,
        pending,
    })
}

impl CapabilityProfile {
    pub(super) fn encode(&self) -> Vec<u8> {
        match self {
            Self::Host => {
                let mut bytes = vec![0; PROFILE_BYTES];
                bytes[0..4].copy_from_slice(&CAPABILITY_VERSION.to_ne_bytes());
                bytes[4..8].copy_from_slice(&CAPABILITY_KIND_HOST.to_ne_bytes());
                bytes
            }
            Self::Renderer(profile) => profile.encode(),
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if !(PROFILE_BYTES..=CAPABILITY_MAX_BYTES).contains(&bytes.len()) {
            return Err(invalid("CastKMS returned an invalid capability size"));
        }
        let version = get_u32(bytes, 0)?;
        let kind = get_u32(bytes, 4)?;
        if version != CAPABILITY_VERSION {
            return Err(unsupported("unsupported CastKMS capability version"));
        }
        if kind == CAPABILITY_KIND_HOST {
            if bytes.len() != PROFILE_BYTES || bytes[8..].iter().any(|byte| *byte != 0) {
                return Err(invalid("CastKMS returned an invalid HOST capability"));
            }
            return Ok(Self::Host);
        }
        let flags = get_u32(bytes, 8)?;
        if kind != CAPABILITY_KIND_RENDERER || flags & !FEATURE_FLAGS != 0 {
            return Err(invalid("CastKMS returned an invalid renderer capability"));
        }
        let count = get_u32(bytes, 12)? as usize;
        if count == 0
            || count > CAPABILITY_MAX_FORMATS
            || bytes.len() != PROFILE_BYTES + count * FORMAT_BYTES
            || bytes[88..PROFILE_BYTES].iter().any(|byte| *byte != 0)
        {
            return Err(invalid("CastKMS returned malformed capability records"));
        }
        let max_output = extent(bytes, 16)?;
        let max_source = extent(bytes, 24)?;
        let min_scale = nonzero(get_u32(bytes, 32)?, "zero minimum scale")?;
        let max_scale = nonzero(get_u32(bytes, 36)?, "zero maximum scale")?;
        let max_layers = nonzero(get_u32(bytes, 40)?, "zero layer limit")?;
        let max_roles = [
            get_u32(bytes, 44)?,
            get_u32(bytes, 48)?,
            get_u32(bytes, 52)?,
        ];
        let max_color_operations = get_u32(bytes, 56)?;
        let max_lut_entries = get_u32(bytes, 60)?;
        let yuv_encodings = get_u32(bytes, 64)?;
        let yuv_ranges = get_u32(bytes, 68)?;
        let min_output = extent(bytes, 72)?;
        let min_source = extent(bytes, 80)?;
        if min_scale > max_scale
            || min_output.width() > max_output.width()
            || min_output.height() > max_output.height()
            || min_source.width() > max_source.width()
            || min_source.height() > max_source.height()
            || max_layers.get() > 24
            || max_roles.iter().any(|count| *count > max_layers.get())
            || max_roles == [0; 3]
            || max_color_operations > 16
            || max_lut_entries > 256
            || yuv_encodings & !YUV_ENCODINGS != 0
            || yuv_ranges & !YUV_RANGES != 0
        {
            return Err(invalid("CastKMS returned unsupported scene limits"));
        }
        let mut formats = Vec::new();
        formats.try_reserve_exact(count).map_err(io::Error::other)?;
        for offset in (PROFILE_BYTES..bytes.len()).step_by(FORMAT_BYTES) {
            formats.push(decode_format(bytes, offset)?);
        }
        Ok(Self::Renderer(RendererCapability {
            flags,
            min_output,
            max_output,
            min_source,
            max_source,
            min_scale,
            max_scale,
            max_layers,
            max_roles,
            max_color_operations,
            max_lut_entries,
            yuv_encodings,
            yuv_ranges,
            formats: formats.into_boxed_slice(),
        }))
    }
}

fn put_format(bytes: &mut Vec<u8>, format: CapabilityFormat) {
    put_u32(bytes, format.fourcc);
    put_u32(bytes, format.plane_count.get());
    let (modifier, explicit) = match format.modifier {
        FormatModifier::Unspecified => (0, false),
        FormatModifier::Explicit(modifier) => (modifier, true),
    };
    put_u64(bytes, modifier);
    let flags = (u32::from(format.provenance.native) * CAPABILITY_FORMAT_NATIVE)
        | (u32::from(format.provenance.imported) * CAPABILITY_FORMAT_IMPORTED)
        | (u32::from(explicit) * CAPABILITY_FORMAT_EXPLICIT_MODIFIER);
    put_u32(bytes, flags);
    put_u32(bytes, format.pitch_alignment.get());
    put_u32(bytes, format.offset_alignment.get());
    put_u32(bytes, format.max_pitch.get());
}

fn decode_format(bytes: &[u8], offset: usize) -> io::Result<CapabilityFormat> {
    let flags = get_u32(bytes, offset + 16)?;
    if flags
        & !(CAPABILITY_FORMAT_NATIVE
            | CAPABILITY_FORMAT_IMPORTED
            | CAPABILITY_FORMAT_EXPLICIT_MODIFIER)
        != 0
    {
        return Err(invalid("CastKMS returned unknown storage capability flags"));
    }
    let modifier = get_u64(bytes, offset + 8)?;
    CapabilityFormat::new(
        get_u32(bytes, offset)?,
        if flags & CAPABILITY_FORMAT_EXPLICIT_MODIFIER != 0 {
            FormatModifier::Explicit(modifier)
        } else if modifier == 0 {
            FormatModifier::Unspecified
        } else {
            return Err(invalid("implicit layout has a nonzero modifier"));
        },
        nonzero(get_u32(bytes, offset + 4)?, "zero memory-plane count")?,
        StorageProvenance::new(
            flags & CAPABILITY_FORMAT_NATIVE != 0,
            flags & CAPABILITY_FORMAT_IMPORTED != 0,
        ),
        nonzero(get_u32(bytes, offset + 20)?, "zero pitch alignment")?,
        nonzero(get_u32(bytes, offset + 24)?, "zero offset alignment")?,
        nonzero(get_u32(bytes, offset + 28)?, "zero maximum pitch")?,
    )
}

fn extent(bytes: &[u8], offset: usize) -> io::Result<Extent> {
    Extent::new(get_u32(bytes, offset)?, get_u32(bytes, offset + 4)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn get_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("truncated CastKMS capability"))?;
    Ok(u32::from_ne_bytes(
        value.try_into().expect("four-byte slice"),
    ))
}

fn get_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("truncated CastKMS capability"))?;
    Ok(u64::from_ne_bytes(
        value.try_into().expect("eight-byte slice"),
    ))
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn nonzero(value: u32, message: &'static str) -> io::Result<NonZeroU32> {
    NonZeroU32::new(value).ok_or_else(|| invalid(message))
}

fn nonzero64(value: u64, message: &'static str) -> io::Result<NonZeroU64> {
    NonZeroU64::new(value).ok_or_else(|| invalid(message))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use castkms_sys::{DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888};

    fn format() -> CapabilityFormat {
        CapabilityFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Explicit(DRM_FORMAT_MOD_LINEAR),
            NonZeroU32::new(1).unwrap(),
            StorageProvenance::new(true, true),
            NonZeroU32::new(4).unwrap(),
            NonZeroU32::new(4).unwrap(),
            NonZeroU32::new(65_536).unwrap(),
        )
        .unwrap()
    }

    fn host_profile_bytes() -> Vec<u8> {
        let mut bytes = vec![0; PROFILE_BYTES];
        bytes[0..4].copy_from_slice(&CAPABILITY_VERSION.to_ne_bytes());
        bytes[4..8].copy_from_slice(&CAPABILITY_KIND_HOST.to_ne_bytes());
        bytes
    }

    fn renderer_profile_bytes() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PROFILE_BYTES + FORMAT_BYTES);
        for value in [
            CAPABILITY_VERSION,
            CAPABILITY_KIND_RENDERER,
            0,
            1,
            1920,
            1080,
            1920,
            1080,
            1 << 16,
            1 << 16,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            1920,
            1080,
            1920,
            1080,
        ] {
            put_u32(&mut bytes, value);
        }
        bytes.resize(PROFILE_BYTES, 0);
        put_u32(&mut bytes, DRM_FORMAT_XRGB8888);
        put_u32(&mut bytes, 1);
        put_u64(&mut bytes, DRM_FORMAT_MOD_LINEAR);
        put_u32(
            &mut bytes,
            CAPABILITY_FORMAT_NATIVE
                | CAPABILITY_FORMAT_IMPORTED
                | CAPABILITY_FORMAT_EXPLICIT_MODIFIER,
        );
        put_u32(&mut bytes, 4);
        put_u32(&mut bytes, 4);
        put_u32(&mut bytes, 65_536);
        bytes
    }

    #[test]
    fn narrow_primary_profile_round_trips() {
        let profile = CapabilityProfile::Renderer(RendererCapability::single_primary(
            Extent::new(1920, 1080).unwrap(),
            format(),
        ));
        let bytes = profile.encode();
        assert_eq!(bytes.len(), PROFILE_BYTES + FORMAT_BYTES);
        let decoded = CapabilityProfile::decode(&bytes).unwrap();
        assert_eq!(decoded, profile);
        let CapabilityProfile::Renderer(decoded) = decoded else {
            panic!("narrow primary profile changed kind");
        };
        assert_eq!(decoded.min_output(), Extent::new(1920, 1080).unwrap());
        assert_eq!(decoded.min_source(), Extent::new(1920, 1080).unwrap());
    }

    #[test]
    fn host_profile_is_canonical() {
        let bytes = host_profile_bytes();
        assert_eq!(bytes.len(), PROFILE_BYTES);
        assert_eq!(
            CapabilityProfile::decode(&bytes).unwrap(),
            CapabilityProfile::Host
        );
        let mut malformed = bytes;
        malformed[127] = 1;
        assert!(CapabilityProfile::decode(&malformed).is_err());
    }

    #[test]
    fn storage_validation_rejects_unusable_tuples() {
        assert!(CapabilityFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Explicit(DRM_FORMAT_MOD_INVALID),
            NonZeroU32::new(1).unwrap(),
            StorageProvenance::new(true, false),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
        )
        .is_err());
        assert!(CapabilityFormat::new(
            DRM_FORMAT_XRGB8888,
            FormatModifier::Unspecified,
            NonZeroU32::new(1).unwrap(),
            StorageProvenance::new(false, false),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(1).unwrap(),
        )
        .is_err());
    }

    #[test]
    fn profile_rejects_inverted_geometry_ranges() {
        let mut bytes = renderer_profile_bytes();
        bytes[72..76].copy_from_slice(&1921_u32.to_ne_bytes());
        assert!(CapabilityProfile::decode(&bytes).is_err());

        let mut bytes = renderer_profile_bytes();
        bytes[84..88].copy_from_slice(&1081_u32.to_ne_bytes());
        assert!(CapabilityProfile::decode(&bytes).is_err());
    }

    #[test]
    fn snapshot_keeps_active_and_pending_identities_together() {
        let active = host_profile_bytes();
        let pending = renderer_profile_bytes();
        let size = SNAPSHOT_BYTES + active.len() + pending.len();
        let mut bytes = Vec::with_capacity(size);
        put_u32(&mut bytes, CAPABILITY_VERSION);
        put_u32(&mut bytes, size as u32);
        put_u32(&mut bytes, castkms_sys::EXECUTION_HOST_V1);
        put_u32(
            &mut bytes,
            CAPABILITY_STATE_PENDING | CAPABILITY_STATE_GATED,
        );
        put_u64(&mut bytes, 7);
        put_u64(&mut bytes, 11);
        put_u64(&mut bytes, 12);
        put_u64(&mut bytes, 13);
        put_u64(&mut bytes, 14);
        put_u32(&mut bytes, SNAPSHOT_BYTES as u32);
        put_u32(&mut bytes, active.len() as u32);
        put_u32(&mut bytes, (SNAPSHOT_BYTES + active.len()) as u32);
        put_u32(&mut bytes, pending.len() as u32);
        bytes.extend_from_slice(&active);
        bytes.extend_from_slice(&pending);
        let snapshot = decode_snapshot(&bytes).unwrap();
        assert_eq!(snapshot.execution_profile(), crate::Profile::HostV1);
        assert_eq!(snapshot.execution_generation().get(), 7);
        assert_eq!(snapshot.active_generation().get(), 11);
        assert_eq!(snapshot.validation_epoch().get(), 14);
        assert_eq!(snapshot.active(), &CapabilityProfile::Host);
        let pending = snapshot.pending().unwrap();
        assert_eq!(pending.generation().get(), 12);
        assert_eq!(pending.transition().get(), 13);
        assert!(pending.gated());
        let CapabilityProfile::Renderer(profile) = pending.profile() else {
            panic!("pending profile is delegated");
        };
        assert_eq!(profile.min_output(), Extent::new(1920, 1080).unwrap());
        assert_eq!(profile.max_output(), Extent::new(1920, 1080).unwrap());
        assert_eq!(profile.min_source(), Extent::new(1920, 1080).unwrap());
        assert_eq!(profile.formats(), &[format()]);
    }
}
