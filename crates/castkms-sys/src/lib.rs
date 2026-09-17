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
pub const RENDERER_VERSION: u32 = 1;
pub const RENDERER_STATE_EMPTY: u32 = 0;
pub const RENDERER_STATE_DRAFT: u32 = 1;
pub const RENDERER_STATE_PUBLISHING: u32 = 2;
pub const RENDERER_STATE_PUBLISHED: u32 = 3;
pub const RENDERER_STATE_WITHDRAWN: u32 = 4;
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
pub const RENDERER_CONSTRAINTS_VERSION: u32 = 1;
pub const RENDERER_CONSTRAINTS_KIND: u32 = 1;
pub const RENDERER_CONSTRAINTS_MAX_FORMATS: usize = 256;
pub const RENDERER_CONSTRAINTS_HEADER_BYTES: usize = 128;
pub const RENDERER_CONSTRAINTS_FORMAT_BYTES: usize = 32;
pub const RENDERER_CONSTRAINTS_MAX_BYTES: usize = RENDERER_CONSTRAINTS_HEADER_BYTES
    + RENDERER_CONSTRAINTS_FORMAT_BYTES * RENDERER_CONSTRAINTS_MAX_FORMATS;
pub const RENDERER_CONSTRAINTS_CROP: u32 = 1 << 0;
pub const RENDERER_CONSTRAINTS_FRACTIONAL: u32 = 1 << 1;
pub const RENDERER_CONSTRAINTS_POSITION: u32 = 1 << 2;
pub const RENDERER_CONSTRAINTS_SCALE: u32 = 1 << 3;
pub const RENDERER_CONSTRAINTS_SRGB: u32 = 1 << 4;
pub const RENDERER_CONSTRAINTS_PLANE_MATRIX: u32 = 1 << 5;
pub const RENDERER_CONSTRAINTS_OUTPUT_MATRIX: u32 = 1 << 6;
pub const RENDERER_CONSTRAINTS_FORMAT_NATIVE: u32 = 1 << 0;
pub const RENDERER_CONSTRAINTS_FORMAT_IMPORTED: u32 = 1 << 1;
pub const RENDERER_CONSTRAINTS_FORMAT_EXPLICIT_MODIFIER: u32 = 1 << 2;
pub const YUV_ENCODING_BT601: u32 = 0;
pub const YUV_ENCODING_BT709: u32 = 1;
pub const YUV_ENCODING_BT2020: u32 = 2;
pub const YUV_RANGE_LIMITED: u32 = 0;
pub const YUV_RANGE_FULL: u32 = 1;
pub const RENDERER_CONSTRAINTS_YUV_ENCODING_BT601: u32 = 1 << YUV_ENCODING_BT601;
pub const RENDERER_CONSTRAINTS_YUV_ENCODING_BT709: u32 = 1 << YUV_ENCODING_BT709;
pub const RENDERER_CONSTRAINTS_YUV_ENCODING_BT2020: u32 = 1 << YUV_ENCODING_BT2020;
pub const RENDERER_CONSTRAINTS_YUV_RANGE_LIMITED: u32 = 1 << YUV_RANGE_LIMITED;
pub const RENDERER_CONSTRAINTS_YUV_RANGE_FULL: u32 = 1 << YUV_RANGE_FULL;

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
    pub state: u32,
    pub constraints_id: u64,
    pub reserved: [u64; 2],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererPrepareOffer {
    pub constraints: u64,
    pub constraints_size: u32,
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub reserved: [u64; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererPublishOffer {
    pub result: u64,
    pub flags: u32,
    pub reserved: u32,
    pub padding: [u64; 2],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererOfferResult {
    pub constraints_id: u64,
    pub reserved: [u64; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererWithdrawOffer {
    pub flags: u32,
    pub reserved: [u32; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererSubmitProbe {
    pub completion_fd: i32,
    pub flags: u32,
    pub reserved: [u64; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsRendererConstraints {
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
pub struct DrmCastkmsRendererConstraintsFormat {
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
    pub constraints_id: u64,
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
    0x40,
    DrmCastkmsRendererQuery
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_prepare_offer,
    b'd',
    0x41,
    DrmCastkmsRendererPrepareOffer
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_withdraw_offer,
    b'd',
    0x44,
    DrmCastkmsRendererWithdrawOffer
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_submit_probe,
    b'd',
    0x42,
    DrmCastkmsRendererSubmitProbe
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_publish_offer,
    b'd',
    0x43,
    DrmCastkmsRendererPublishOffer
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_release_source,
    b'd',
    0x48,
    DrmCastkmsRendererReleaseSource
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_dequeue_scene,
    b'd',
    0x47,
    DrmCastkmsRendererDequeueScene
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_register_image,
    b'd',
    0x45,
    DrmCastkmsRendererRegisterImage
);
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_renderer_unregister_image,
    b'd',
    0x46,
    DrmCastkmsRendererUnregisterImage
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_operations_match_the_uapi_layouts() {
        assert_eq!(RENDERER_VERSION, 1);
        assert_eq!(RENDERER_STATE_EMPTY, 0);
        assert_eq!(RENDERER_STATE_DRAFT, 1);
        assert_eq!(RENDERER_STATE_PUBLISHING, 2);
        assert_eq!(RENDERER_STATE_PUBLISHED, 3);
        assert_eq!(RENDERER_STATE_WITHDRAWN, 4);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererQuery>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererQuery>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererPrepareOffer>(), 48);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererPrepareOffer, constraints),
            0
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererPublishOffer>(), 32);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererPublishOffer, result),
            0
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererOfferResult>(), 32);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererWithdrawOffer>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererSubmitProbe>(), 32);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererSubmitProbe, completion_fd),
            0
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererConstraints>(), 128);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererConstraints>(), 4);
        assert_eq!(
            std::mem::size_of::<DrmCastkmsRendererConstraintsFormat>(),
            32
        );
        assert_eq!(
            std::mem::align_of::<DrmCastkmsRendererConstraintsFormat>(),
            8
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
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererScene>(), 56);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererScene>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsRendererScene, job_id), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererScene, constraints_id),
            16
        );
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsRendererScene, producer_fd),
            44
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
