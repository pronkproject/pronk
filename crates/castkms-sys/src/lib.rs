//! Checked-in Rust bindings for the CastKMS renderer and required DRM UAPIs.

use std::ffi::c_char;

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;
pub const DRM_MODE_UNKNOWN_CONNECTION: u32 = 3;

pub const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub const DRM_MODE_CONNECTOR_WRITEBACK: u32 = 18;

pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const DRM_FORMAT_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
pub const DRM_FORMAT_XRGB2101010: u32 = u32::from_le_bytes(*b"XR30");
pub const DRM_FORMAT_XBGR2101010: u32 = u32::from_le_bytes(*b"XB30");
pub const DRM_FORMAT_ARGB2101010: u32 = u32::from_le_bytes(*b"AR30");
pub const DRM_FORMAT_ABGR2101010: u32 = u32::from_le_bytes(*b"AB30");
pub const DRM_FORMAT_RGB565: u32 = u32::from_le_bytes(*b"RG16");
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
pub const RENDERER_VERSION: u32 = 9;
pub const TRANSITION_PROPERTY: &str = "CASTKMS_TRANSITION";
pub const EXECUTION_PROPERTY: &str = "CASTKMS_EXECUTION";
pub const RENDERER_PROBE_PRIVATE: u32 = 1;
pub const RENDERER_PROBE_STARTUP_IMAGE: u32 = 2;
pub const RENDERER_RELEASE_NO_ACCESS: u32 = 1;
pub const RENDERER_RELEASE_CPU_DONE: u32 = 2;
pub const RENDERER_RELEASE_SUBMITTED: u32 = 3;
pub const RENDERER_MAX_PLANES: usize = 4;
pub const RENDERER_SCENE_VERSION: u32 = 1;
pub const RENDERER_SCENE_MAX_BYTES: usize = 65_536;
pub const RENDERER_SCENE_MAX_LAYERS: usize = 24;
pub const RENDERER_SCENE_MAX_COLOR_OPS: usize = 16;
pub const RENDERER_LAYER_PRIMARY: u32 = 0;
pub const RENDERER_LAYER_OVERLAY: u32 = 1;
pub const RENDERER_LAYER_CURSOR: u32 = 2;
pub const RENDERER_COLOR_BYPASS: u32 = 0;
pub const RENDERER_COLOR_SRGB_EOTF: u32 = 1;
pub const RENDERER_COLOR_SRGB_INVERSE_EOTF: u32 = 2;
pub const RENDERER_COLOR_MATRIX: u32 = 3;
pub const RENDERER_COLOR_LUT: u32 = 4;
pub const EXECUTION_HOST_V1: u32 = 1;
pub const EXECUTION_GPU_V1: u32 = 2;
pub const CAPABILITY_VERSION: u32 = 2;
pub const CAPABILITY_KIND_HOST: u32 = 1;
pub const CAPABILITY_KIND_RENDERER: u32 = 2;
pub const CAPABILITY_MAX_FORMATS: usize = 256;
pub const CAPABILITY_MAX_BYTES: usize = 128 + 32 * CAPABILITY_MAX_FORMATS;
pub const CAPABILITY_QUERY_MAX_BYTES: usize = 72 + 2 * CAPABILITY_MAX_BYTES;
pub const CAPABILITY_PROFILE_CROP: u32 = 1 << 0;
pub const CAPABILITY_PROFILE_FRACTIONAL: u32 = 1 << 1;
pub const CAPABILITY_PROFILE_POSITION: u32 = 1 << 2;
pub const CAPABILITY_PROFILE_SCALE: u32 = 1 << 3;
pub const CAPABILITY_PROFILE_SRGB: u32 = 1 << 4;
pub const CAPABILITY_PROFILE_PLANE_MATRIX: u32 = 1 << 5;
pub const CAPABILITY_PROFILE_OUTPUT_MATRIX: u32 = 1 << 6;
pub const CAPABILITY_FORMAT_NATIVE: u32 = 1 << 0;
pub const CAPABILITY_FORMAT_IMPORTED: u32 = 1 << 1;
pub const CAPABILITY_FORMAT_EXPLICIT_MODIFIER: u32 = 1 << 2;
pub const CAPABILITY_STATE_PENDING: u32 = 1 << 0;
pub const CAPABILITY_STATE_GATED: u32 = 1 << 1;
pub const YUV_ENCODING_BT601: u32 = 0;
pub const YUV_ENCODING_BT709: u32 = 1;
pub const YUV_ENCODING_BT2020: u32 = 2;
pub const YUV_RANGE_LIMITED: u32 = 0;
pub const YUV_RANGE_FULL: u32 = 1;
pub const CAPABILITY_YUV_ENCODING_BT601: u32 = 1 << YUV_ENCODING_BT601;
pub const CAPABILITY_YUV_ENCODING_BT709: u32 = 1 << YUV_ENCODING_BT709;
pub const CAPABILITY_YUV_ENCODING_BT2020: u32 = 1 << YUV_ENCODING_BT2020;
pub const CAPABILITY_YUV_RANGE_LIMITED: u32 = 1 << YUV_RANGE_LIMITED;
pub const CAPABILITY_YUV_RANGE_FULL: u32 = 1 << YUV_RANGE_FULL;

/// Native-pointer layout used by the standard DRM `VERSION` ioctl.
///
/// Unlike driver-private DRM UAPIs, this standard structure intentionally uses
/// native `size_t` and pointer fields.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: usize,
    pub name: *mut c_char,
    pub date_len: usize,
    pub date: *mut c_char,
    pub desc_len: usize,
    pub desc: *mut c_char,
}

impl Default for DrmVersion {
    fn default() -> Self {
        Self {
            version_major: 0,
            version_minor: 0,
            version_patchlevel: 0,
            name_len: 0,
            name: std::ptr::null_mut(),
            date_len: 0,
            date: std::ptr::null_mut(),
            desc_len: 0,
            desc: std::ptr::null_mut(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeCardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeGetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub mode_type: u32,
    pub name: [u8; 32],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererQuery {
    pub version: u32,
    pub flags: u32,
    pub profile: u32,
    pub reserved: u32,
    pub generation: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererTakeover {
    pub candidate_id: u64,
    pub execution_generation: u64,
    pub profile: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub mode_flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererBeginTakeover {
    pub expected_generation: u64,
    pub result: u64,
    pub flags: u32,
    pub reserved: [u32; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererAbortTakeover {
    pub candidate_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererSnapshot {
    pub dma_buf_fd: i32,
    pub format: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub offset: u32,
    pub content_serial: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererGetSnapshot {
    pub candidate_id: u64,
    pub result: u64,
    pub flags: u32,
    pub reserved: [u32; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererSubmitProbe {
    pub candidate_id: u64,
    pub completion_fd: i32,
    pub source: u32,
    pub flags: u32,
    pub reserved: [u32; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererCommitTakeover {
    pub candidate_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCapabilityProfile {
    pub version: u32,
    pub kind: u32,
    pub flags: u32,
    pub format_count: u32,
    pub max_output: [u32; 2],
    pub max_source: [u32; 2],
    pub min_scale: u32,
    pub max_scale: u32,
    pub max_layers: u32,
    pub max_roles: [u32; 3],
    pub max_color_operations: u32,
    pub max_lut_entries: u32,
    pub yuv_encodings: u32,
    pub yuv_ranges: u32,
    pub min_output: [u32; 2],
    pub min_source: [u32; 2],
    pub reserved: [u32; 10],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCapabilityFormat {
    pub fourcc: u32,
    pub plane_count: u32,
    pub modifier: u64,
    pub flags: u32,
    pub pitch_alignment: u32,
    pub offset_alignment: u32,
    pub max_pitch: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererRegisterProfile {
    pub candidate_id: u64,
    pub profile: u64,
    pub result: u64,
    pub profile_size: u32,
    pub flags: u32,
    pub reserved: [u64; 2],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererProfileResult {
    pub transition: u64,
    pub capability_generation: u64,
    pub execution_generation: u64,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererCapabilities {
    pub version: u32,
    pub size: u32,
    pub execution_profile: u32,
    pub flags: u32,
    pub execution_generation: u64,
    pub active_generation: u64,
    pub pending_generation: u64,
    pub transition: u64,
    pub validation_epoch: u64,
    pub active_offset: u32,
    pub active_size: u32,
    pub pending_offset: u32,
    pub pending_size: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererQueryCapabilities {
    pub result: u64,
    pub capacity: u32,
    pub flags: u32,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererSourcePlane {
    pub dma_buf_fd: i32,
    pub pitch: u32,
    pub offset: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererReleaseSource {
    pub job_id: u64,
    pub completion_fd: i32,
    pub kind: u32,
    pub flags: u32,
    pub reserved: [u32; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererDequeueScene {
    pub result: u64,
    pub image_id: u64,
    pub capacity: u32,
    pub flags: u32,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererRegisterImage {
    pub image_id: u64,
    pub buffers: u64,
    pub width: u32,
    pub height: u32,
    pub num_buffers: u32,
    pub flags: u32,
    pub reserved: [u64; 2],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererUnregisterImage {
    pub image_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererScene {
    pub version: u32,
    pub bytes: u32,
    pub job_id: u64,
    pub content_serial: u64,
    pub width: u32,
    pub height: u32,
    pub layer_count: u32,
    pub producer_fd: i32,
    pub output_color_count: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererLayer {
    pub bytes: u32,
    pub kind: u32,
    pub zpos: u32,
    pub format: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub source: [u32; 4],
    pub position: [i32; 2],
    pub destination: [u32; 2],
    pub color_encoding: u32,
    pub color_range: u32,
    pub plane_count: u32,
    pub color_count: u32,
    pub planes: [DrmCastkmsRendererSourcePlane; RENDERER_MAX_PLANES],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererColor {
    pub kind: u32,
    pub payload_bytes: u32,
}

nix::ioctl_readwrite!(drm_ioctl_version, b'd', 0x00, DrmVersion);
nix::ioctl_readwrite!(drm_ioctl_mode_getresources, b'd', 0xa0, DrmModeCardRes);
nix::ioctl_readwrite!(drm_ioctl_mode_getencoder, b'd', 0xa6, DrmModeGetEncoder);
nix::ioctl_readwrite!(drm_ioctl_mode_getconnector, b'd', 0xa7, DrmModeGetConnector);

nix::ioctl_read!(
    drm_ioctl_castkms_renderer_query,
    b'd',
    0x44,
    DrmCastkmsRendererQuery
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_begin_takeover,
    b'd',
    0x45,
    DrmCastkmsRendererBeginTakeover
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_abort_takeover,
    b'd',
    0x46,
    DrmCastkmsRendererAbortTakeover
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_get_snapshot,
    b'd',
    0x47,
    DrmCastkmsRendererGetSnapshot
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_submit_probe,
    b'd',
    0x48,
    DrmCastkmsRendererSubmitProbe
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_commit_takeover,
    b'd',
    0x49,
    DrmCastkmsRendererCommitTakeover
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_release_source,
    b'd',
    0x4b,
    DrmCastkmsRendererReleaseSource
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_dequeue_scene,
    b'd',
    0x4c,
    DrmCastkmsRendererDequeueScene
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_register_profile,
    b'd',
    0x4d,
    DrmCastkmsRendererRegisterProfile
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_query_capabilities,
    b'd',
    0x4e,
    DrmCastkmsRendererQueryCapabilities
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_register_image,
    b'd',
    0x4f,
    DrmCastkmsRendererRegisterImage
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_unregister_image,
    b'd',
    0x50,
    DrmCastkmsRendererUnregisterImage
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_operations_match_the_uapi_layouts() {
        assert_eq!(RENDERER_VERSION, 9);
        assert_eq!(RENDERER_PROBE_PRIVATE, 1);
        assert_eq!(RENDERER_PROBE_STARTUP_IMAGE, 2);
        assert_eq!(EXECUTION_HOST_V1, 1);
        assert_eq!(EXECUTION_GPU_V1, 2);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererQuery>(), 24);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererQuery>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererTakeover>(), 40);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererTakeover, execution_generation),
            8
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererBeginTakeover>(), 32);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererBeginTakeover, result),
            8
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererAbortTakeover>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererSnapshot>(), 48);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererSnapshot, content_serial),
            32
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererGetSnapshot>(), 32);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererGetSnapshot, result),
            8
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererSubmitProbe>(), 32);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererSubmitProbe, completion_fd),
            8
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererCommitTakeover>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsCapabilityProfile>(), 128);
        assert_eq!(std::mem::align_of::<DrmCastkmsCapabilityProfile>(), 4);
        assert_eq!(std::mem::size_of::<DrmCastkmsCapabilityFormat>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsCapabilityFormat>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererRegisterProfile>(), 48);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererProfileResult>(), 32);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererCapabilities>(), 72);
        assert_eq!(
            std::mem::size_of::<DrmCastkmsRendererQueryCapabilities>(),
            24
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererSourcePlane>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererReleaseSource>(), 32);
        assert_eq!(RENDERER_SCENE_VERSION, 1);
        assert_eq!(RENDERER_SCENE_MAX_BYTES, 65_536);
        assert_eq!(RENDERER_SCENE_MAX_LAYERS, 24);
        assert_eq!(RENDERER_SCENE_MAX_COLOR_OPS, 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererDequeueScene>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererDequeueScene>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererDequeueScene, image_id),
            8
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererRegisterImage>(), 48);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererRegisterImage>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererUnregisterImage>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererScene>(), 48);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererScene>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsRendererScene, job_id), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererScene, producer_fd),
            36
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererLayer>(), 144);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererLayer>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsRendererLayer, modifier), 16);
        assert_eq!(std::mem::offset_of!(DrmCastkmsRendererLayer, planes), 80);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererColor>(), 8);
    }

    #[test]
    fn output_discovery_operations_match_the_uapi_layouts() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<DrmVersion>(), 64);
            assert_eq!(std::mem::align_of::<DrmVersion>(), 8);
            assert_eq!(std::mem::offset_of!(DrmVersion, name_len), 16);
            assert_eq!(std::mem::offset_of!(DrmVersion, name), 24);
        }
        assert_eq!(std::mem::size_of::<DrmModeCardRes>(), 64);
        assert_eq!(std::mem::align_of::<DrmModeCardRes>(), 8);
        assert_eq!(std::mem::offset_of!(DrmModeCardRes, count_fbs), 32);
        assert_eq!(std::mem::size_of::<DrmModeGetConnector>(), 80);
        assert_eq!(std::mem::align_of::<DrmModeGetConnector>(), 8);
        assert_eq!(std::mem::offset_of!(DrmModeGetConnector, connector_id), 48);
        assert_eq!(std::mem::offset_of!(DrmModeGetConnector, pad), 76);
        assert_eq!(std::mem::size_of::<DrmModeGetEncoder>(), 20);
        assert_eq!(std::mem::align_of::<DrmModeGetEncoder>(), 4);
        assert_eq!(std::mem::offset_of!(DrmModeGetEncoder, crtc_id), 8);
    }
}
