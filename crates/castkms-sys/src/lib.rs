//! Minimal checked-in Rust bindings for the CastKMS 0.12 grant, attachment,
//! capture, event, output-discovery, and required standard DRM UAPIs.

use std::ffi::c_char;

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;
pub const DRM_MODE_UNKNOWN_CONNECTION: u32 = 3;

pub const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub const DRM_MODE_CONNECTOR_WRITEBACK: u32 = 18;

pub const CAPTURE_UAPI_MAJOR: u16 = 0;
pub const CAPTURE_UAPI_MINOR: u16 = 12;
pub const CAPTURE_MAX_DISPLAY_NAME_SIZE: usize = 79;

pub const CAPTURE_CAP_SYNCOBJ_TIMELINE: u64 = 1 << 0;
pub const CAPTURE_CAP_IMPLICIT_SYNC: u64 = 1 << 1;
pub const CAPTURE_CAP_DMA_BUF_IMPORT: u64 = 1 << 2;
pub const CAPTURE_CAP_GRANT_FD: u64 = 1 << 3;
pub const CAPTURE_CAP_GRANT_CONTROL_FD: u64 = 1 << 4;
pub const CAPTURE_CAPS_MASK: u64 = CAPTURE_CAP_SYNCOBJ_TIMELINE
    | CAPTURE_CAP_IMPLICIT_SYNC
    | CAPTURE_CAP_DMA_BUF_IMPORT
    | CAPTURE_CAP_GRANT_FD
    | CAPTURE_CAP_GRANT_CONTROL_FD;

pub const GRANT_CAPTURE_PIXELS: u32 = 1 << 0;
pub const GRANT_MANAGE_ATTACHMENT: u32 = 1 << 1;
pub const GRANT_UPDATE_EDID: u32 = 1 << 2;
pub const GRANT_READ_CURSOR: u32 = 1 << 3;
pub const GRANT_MANAGE_CEC: u32 = 1 << 4;
pub const GRANT_CAPTURE_AUDIO: u32 = 1 << 5;

pub const DISPLAY_V1_RIGHTS: u32 =
    GRANT_CAPTURE_PIXELS | GRANT_MANAGE_ATTACHMENT | GRANT_UPDATE_EDID | GRANT_READ_CURSOR;
pub const DISPLAY_CEC_V1_RIGHTS: u32 = DISPLAY_V1_RIGHTS | GRANT_MANAGE_CEC;
pub const DISPLAY_AUDIO_V1_RIGHTS: u32 = DISPLAY_V1_RIGHTS | GRANT_CAPTURE_AUDIO;
pub const DISPLAY_CEC_AUDIO_V1_RIGHTS: u32 = DISPLAY_CEC_V1_RIGHTS | GRANT_CAPTURE_AUDIO;

pub const AUDIO_FORMAT_S16_LE: u32 = 1;
pub const AUDIO_TAP_RATE: u32 = 48_000;
pub const AUDIO_TAP_CHANNELS: u32 = 2;
pub const AUDIO_TAP_FRAME_BYTES: u32 = 4;

pub const GRANT_CREATE_ADMIN: u32 = 1 << 0;
pub const GRANT_CREATE_DELEGATED: u32 = 1 << 1;
pub const GRANT_CREATE_FLAGS_MASK: u32 = GRANT_CREATE_ADMIN | GRANT_CREATE_DELEGATED;

pub const GRANT_FLAG_ADMIN: u32 = 1 << 0;
pub const GRANT_FLAG_DELEGATED: u32 = 1 << 1;
pub const GRANT_FLAGS_MASK: u32 = GRANT_FLAG_ADMIN | GRANT_FLAG_DELEGATED;

pub const GRANT_STATE_PENDING: u32 = 0;
pub const GRANT_STATE_ACTIVE: u32 = 1;
pub const GRANT_STATE_SUSPENDED_NO_MASTER: u32 = 2;
pub const GRANT_STATE_SUSPENDED_OTHER_MASTER: u32 = 3;
pub const GRANT_STATE_SUSPENDED_FOREIGN_CONTENT: u32 = 4;
pub const GRANT_STATE_REVOKED: u32 = 5;

pub const CAPTURE_EVENT_FRAME: u32 = 0x8000_0000;
pub const CAPTURE_EVENT_GRANT_REVOKED: u32 = 0x8000_0003;
pub const CAPTURE_EVENT_GRANT_STATE: u32 = 0x8000_0004;

pub const CEC_UAPI_MAJOR: u32 = 0;
pub const CEC_UAPI_MINOR: u32 = 1;

pub const CEC_CAP_ASYNC_TX: u64 = 1 << 0;
pub const CEC_CAP_RX_INJECT: u64 = 1 << 1;
pub const CEC_CAP_STATE_EVENTS: u64 = 1 << 2;
pub const CEC_CAP_TRANSPORT_STATE: u64 = 1 << 3;
pub const CEC_CAP_EDID_PHYS_ADDR: u64 = 1 << 4;
pub const CEC_CAPS_MASK: u64 = CEC_CAP_ASYNC_TX
    | CEC_CAP_RX_INJECT
    | CEC_CAP_STATE_EVENTS
    | CEC_CAP_TRANSPORT_STATE
    | CEC_CAP_EDID_PHYS_ADDR;

pub const CEC_TRANSPORT_ONLINE: u32 = 1 << 0;

pub const CEC_STATE_TRANSPORT_ONLINE: u32 = 1 << 0;
pub const CEC_STATE_MONITOR_ATTACHED: u32 = 1 << 1;
pub const CEC_STATE_ADAPTER_ENABLED: u32 = 1 << 2;
pub const CEC_STATE_MASK: u32 =
    CEC_STATE_TRANSPORT_ONLINE | CEC_STATE_MONITOR_ATTACHED | CEC_STATE_ADAPTER_ENABLED;

pub const CEC_EVENT_TX: u32 = 0x8000_0001;
pub const CEC_EVENT_STATE: u32 = 0x8000_0002;

pub const CEC_TX_STATUS_OK: u8 = 1 << 0;
pub const CEC_TX_STATUS_ARB_LOST: u8 = 1 << 1;
pub const CEC_TX_STATUS_NACK: u8 = 1 << 2;
pub const CEC_TX_STATUS_LOW_DRIVE: u8 = 1 << 3;
pub const CEC_TX_STATUS_ERROR: u8 = 1 << 4;
pub const CEC_TX_STATUS_MAX_RETRIES: u8 = 1 << 5;
pub const CEC_TX_STATUS_ABORTED: u8 = 1 << 6;
pub const CEC_TX_STATUS_TIMEOUT: u8 = 1 << 7;

pub const CAPTURE_FRAME_FULL_DAMAGE: u32 = 1 << 0;
pub const CAPTURE_FRAME_MODE_CHANGED: u32 = 1 << 1;

pub const CURSOR_VISIBLE: u32 = 1 << 0;
pub const CURSOR_IMAGE_CHANGED: u32 = 1 << 1;

pub const CAPTURE_START_EXCLUSIVE: u32 = 1 << 0;
pub const CAPTURE_START_EXCLUDE_CURSOR: u32 = 1 << 1;

pub const CAPTURE_BUFFER_IMPLICIT_SYNC: u32 = 1 << 0;
pub const CAPTURE_BUFFER_EXPLICIT_SYNC: u32 = 1 << 1;

pub const CAPTURE_QUEUE_IMPLICIT_SYNC: u32 = 1 << 0;
pub const CAPTURE_QUEUE_EXPLICIT_SYNC: u32 = 1 << 1;

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
pub const DRM_CLOEXEC: u32 = 0x0008_0000;
pub const DRM_RDWR: u32 = 0x0000_0002;
pub const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_NONE: u32 = 0;
pub const DMA_BUF_EXPORT_SYNC_WRITE: u32 = 2;
pub const DMA_BUF_SYNC_READ: u64 = 1;
pub const DMA_BUF_SYNC_WRITE: u64 = 2;
pub const DMA_BUF_SYNC_START: u64 = 0;
pub const DMA_BUF_SYNC_END: u64 = 1 << 2;

pub const RENDERER_VERSION: u32 = 8;
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
/// Unlike driver-private DRM UAPIs, this legacy structure intentionally uses
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
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeModeInfo,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeDestroyDumb {
    pub handle: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DmaBufExportSyncFile {
    pub flags: u32,
    pub fd: i32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DmaBufSync {
    pub flags: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmSyncobjCreate {
    pub handle: u32,
    pub flags: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmSyncobjDestroy {
    pub handle: u32,
    pub pad: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
    pub point: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmSyncobjTimelineArray {
    pub handles: u64,
    pub points: u64,
    pub count_handles: u32,
    pub flags: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmSyncobjEventfd {
    pub handle: u32,
    pub flags: u32,
    pub point: u64,
    pub fd: i32,
    pub pad: u32,
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
    pub capacity: u32,
    pub flags: u32,
    pub reserved: u64,
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureFormat {
    pub format: u32,
    pub flags: u32,
    pub modifier: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureQueryCaps {
    pub uapi_major: u32,
    pub uapi_minor: u32,
    pub crtc_id: u32,
    pub format_count: u32,
    pub flags: u64,
    pub formats_ptr: u64,
    pub max_registered_buffers: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureStart {
    pub crtc_id: u32,
    pub flags: u32,
    pub stream_id: u32,
    pub reserved: u32,
    pub mode_generation: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureStop {
    pub stream_id: u32,
    pub flags: u32,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureRegisterBuffer {
    pub stream_id: u32,
    pub fb_id: u32,
    pub ready_syncobj_handle: u32,
    pub reuse_syncobj_handle: u32,
    pub flags: u32,
    pub buffer_id: u32,
    pub mode_generation: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureUnregisterBuffer {
    pub stream_id: u32,
    pub buffer_id: u32,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureQueueBuffer {
    pub stream_id: u32,
    pub buffer_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
    pub mode_generation: u64,
    pub ready_point: u64,
    pub reuse_point: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmEvent {
    pub event_type: u32,
    pub length: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmEventCastkmsGrantRevoked {
    pub base: DrmEvent,
    pub grant_id: u32,
    pub status: i32,
    pub timestamp_ns: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmEventCastkmsGrantState {
    pub base: DrmEvent,
    pub grant_id: u32,
    pub state: u32,
    pub status: i32,
    pub reserved: u32,
    pub timestamp_ns: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmEventCastkmsCaptureFrame {
    pub base: DrmEvent,
    pub user_data: u64,
    pub sequence: u64,
    pub timestamp_ns: i64,
    pub mode_generation: u64,
    pub stream_id: u32,
    pub buffer_id: u32,
    pub status: i32,
    pub flags: u32,
    pub dropped_frames: u32,
    pub damage_x: i32,
    pub damage_y: i32,
    pub damage_width: u32,
    pub damage_height: u32,
    pub cursor_serial: u32,
    pub cursor_flags: u32,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_hotspot_x: u32,
    pub cursor_hotspot_y: u32,
    pub cursor_width: u32,
    pub cursor_height: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureSetOutputEdid {
    pub connector_id: u32,
    pub flags: u32,
    pub edid_size: u32,
    pub reserved: u32,
    pub edid_ptr: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureAttachMonitor {
    pub connector_id: u32,
    pub flags: u32,
    pub edid_size: u32,
    pub display_name_size: u32,
    pub edid_ptr: u64,
    pub display_name_ptr: u64,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCaptureDetachMonitor {
    pub connector_id: u32,
    pub flags: u32,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCreateGrant {
    pub connector_id: u32,
    pub rights: u32,
    pub flags: u32,
    pub fd: i32,
    pub grant_id: u32,
    pub fd_flags: u32,
    pub control_fd: i32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsGetGrant {
    pub grant_id: u32,
    pub connector_id: u32,
    pub rights: u32,
    pub state: u32,
    pub flags: u32,
    pub output_index: u32,
    pub reserved: u64,
}

/// Read-only connector-to-output mapping exposed by CastKMS.
///
/// The query is valid on an ordinary DRM file and does not require a grant.
/// `output_index` is therefore the authoritative stable slot identity used
/// during pre-authorization output discovery.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsGetOutput {
    pub connector_id: u32,
    pub flags: u32,
    pub output_index: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsOpenAudioTap {
    pub connector_id: u32,
    pub flags: u32,
    pub fd: i32,
    pub fd_flags: u32,
    pub format: u32,
    pub rate: u32,
    pub channels: u32,
    pub frame_bytes: u32,
    pub buffer_frames: u64,
    pub reserved: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecQueryCaps {
    pub connector_id: u32,
    pub flags: u32,
    pub uapi_major: u32,
    pub uapi_minor: u32,
    pub capabilities: u64,
    pub max_msg_size: u32,
    pub output_index: u32,
    pub has_adapter: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecBindTransport {
    pub connector_id: u32,
    pub flags: u32,
    pub transport_id: u32,
    pub reserved: u32,
    pub transport_generation: u64,
    pub state_generation: u64,
    pub output_index: u32,
    pub state_flags: u32,
    pub phys_addr: u16,
    pub logical_addr_mask: u16,
    pub pad0: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecUnbindTransport {
    pub connector_id: u32,
    pub transport_id: u32,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecSetTransportState {
    pub connector_id: u32,
    pub transport_id: u32,
    pub flags: u32,
    pub reserved: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecTxComplete {
    pub connector_id: u32,
    pub transport_id: u32,
    pub transport_generation: u64,
    pub cookie: u64,
    pub status: u8,
    pub arb_lost_cnt: u8,
    pub nack_cnt: u8,
    pub low_drive_cnt: u8,
    pub error_cnt: u8,
    pub reserved: [u8; 3],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecReceive {
    pub connector_id: u32,
    pub transport_id: u32,
    pub transport_generation: u64,
    pub length: u8,
    pub flags: u8,
    pub msg: [u8; 16],
    pub reserved: u8,
    pub pad0: [u8; 5],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmCastkmsCecGetState {
    pub connector_id: u32,
    pub transport_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub transport_generation: u64,
    pub state_generation: u64,
    pub state_flags: u32,
    pub output_index: u32,
    pub phys_addr: u16,
    pub logical_addr_mask: u16,
    pub pad0: u32,
    pub pending_cookie: u64,
    pub stats_tx_submitted: u64,
    pub stats_tx_completed: u64,
    pub stats_tx_nack: u64,
    pub stats_tx_error: u64,
    pub stats_tx_timeout: u64,
    pub stats_rx: u64,
    pub stats_invalid: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DrmEventCastkmsCecTx {
    pub base: DrmEvent,
    pub transport_id: u32,
    pub pad0: u32,
    pub transport_generation: u64,
    pub state_generation: u64,
    pub cookie: u64,
    pub connector_id: u32,
    pub output_index: u32,
    pub attempts: u8,
    pub length: u8,
    pub msg: [u8; 16],
    pub reserved: u16,
    pub signal_free_time: u32,
}

nix::ioctl_readwrite!(drm_ioctl_version, b'd', 0x00, DrmVersion);
nix::ioctl_write_ptr!(drm_ioctl_auth_magic, b'd', 0x11, u32);
nix::ioctl_readwrite!(drm_ioctl_mode_getresources, b'd', 0xa0, DrmModeCardRes);
nix::ioctl_none!(drm_ioctl_drop_master, b'd', 0x1f);
nix::ioctl_readwrite!(drm_ioctl_prime_handle_to_fd, b'd', 0x2d, DrmPrimeHandle);
nix::ioctl_readwrite!(drm_ioctl_mode_getcrtc, b'd', 0xa1, DrmModeCrtc);
nix::ioctl_readwrite!(drm_ioctl_mode_getencoder, b'd', 0xa6, DrmModeGetEncoder);
nix::ioctl_readwrite!(drm_ioctl_mode_getconnector, b'd', 0xa7, DrmModeGetConnector);
nix::ioctl_readwrite!(drm_ioctl_mode_rmfb, b'd', 0xaf, u32);
nix::ioctl_readwrite!(drm_ioctl_mode_create_dumb, b'd', 0xb2, DrmModeCreateDumb);
nix::ioctl_readwrite!(drm_ioctl_mode_destroy_dumb, b'd', 0xb4, DrmModeDestroyDumb);
nix::ioctl_readwrite!(drm_ioctl_mode_addfb2, b'd', 0xb8, DrmModeFbCmd2);
nix::ioctl_readwrite!(drm_ioctl_syncobj_create, b'd', 0xbf, DrmSyncobjCreate);
nix::ioctl_readwrite!(drm_ioctl_syncobj_destroy, b'd', 0xc0, DrmSyncobjDestroy);
nix::ioctl_readwrite!(drm_ioctl_syncobj_handle_to_fd, b'd', 0xc1, DrmSyncobjHandle);
nix::ioctl_readwrite!(
    drm_ioctl_syncobj_timeline_signal,
    b'd',
    0xcd,
    DrmSyncobjTimelineArray
);
nix::ioctl_readwrite!(drm_ioctl_syncobj_eventfd, b'd', 0xcf, DrmSyncobjEventfd);
nix::ioctl_readwrite!(
    dma_buf_ioctl_export_sync_file,
    b'b',
    0x02,
    DmaBufExportSyncFile
);
nix::ioctl_write_ptr!(dma_buf_ioctl_sync, b'b', 0x00, DmaBufSync);

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

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_QUERY_CAPS (0x00).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_capture_query_caps,
    b'd',
    0x40,
    DrmCastkmsCaptureQueryCaps
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_START (0x01).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_capture_start,
    b'd',
    0x41,
    DrmCastkmsCaptureStart
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_STOP (0x02).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_capture_stop,
    b'd',
    0x42,
    DrmCastkmsCaptureStop
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_REGISTER_BUFFER (0x03).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_capture_register_buffer,
    b'd',
    0x43,
    DrmCastkmsCaptureRegisterBuffer
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_UNREGISTER_BUFFER (0x04).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_capture_unregister_buffer,
    b'd',
    0x44,
    DrmCastkmsCaptureUnregisterBuffer
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_QUEUE_BUFFER (0x05).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_capture_queue_buffer,
    b'd',
    0x45,
    DrmCastkmsCaptureQueueBuffer
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_SET_OUTPUT_EDID (0x06).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_capture_set_output_edid,
    b'd',
    0x46,
    DrmCastkmsCaptureSetOutputEdid
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_ATTACH_MONITOR (0x07).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_capture_attach_monitor,
    b'd',
    0x47,
    DrmCastkmsCaptureAttachMonitor
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CAPTURE_DETACH_MONITOR (0x08).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_capture_detach_monitor,
    b'd',
    0x48,
    DrmCastkmsCaptureDetachMonitor
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_QUERY_CAPS (0x0a).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_cec_query_caps,
    b'd',
    0x4a,
    DrmCastkmsCecQueryCaps
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_BIND_TRANSPORT (0x0b).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_cec_bind_transport,
    b'd',
    0x4b,
    DrmCastkmsCecBindTransport
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_UNBIND_TRANSPORT (0x0c).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_cec_unbind_transport,
    b'd',
    0x4c,
    DrmCastkmsCecUnbindTransport
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_SET_TRANSPORT_STATE (0x0d).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_cec_set_transport_state,
    b'd',
    0x4d,
    DrmCastkmsCecSetTransportState
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_TX_COMPLETE (0x0e).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_cec_tx_complete,
    b'd',
    0x4e,
    DrmCastkmsCecTxComplete
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_RECEIVE (0x0f).
nix::ioctl_write_ptr!(
    drm_ioctl_castkms_cec_receive,
    b'd',
    0x4f,
    DrmCastkmsCecReceive
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CEC_GET_STATE (0x10).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_cec_get_state,
    b'd',
    0x50,
    DrmCastkmsCecGetState
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_CREATE_GRANT (0x11).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_create_grant,
    b'd',
    0x51,
    DrmCastkmsCreateGrant
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_GET_GRANT (0x13).
nix::ioctl_readwrite!(drm_ioctl_castkms_get_grant, b'd', 0x53, DrmCastkmsGetGrant);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_GET_OUTPUT (0x14).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_get_output,
    b'd',
    0x54,
    DrmCastkmsGetOutput
);

// DRM_COMMAND_BASE (0x40) + DRM_CASTKMS_OPEN_AUDIO_TAP (0x15).
nix::ioctl_readwrite!(
    drm_ioctl_castkms_open_audio_tap,
    b'd',
    0x55,
    DrmCastkmsOpenAudioTap
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_query_matches_the_uapi_layout() {
        assert_eq!(CAPTURE_UAPI_MAJOR, 0);
        assert_eq!(CAPTURE_UAPI_MINOR, 12);
        assert_eq!(CAPTURE_CAP_GRANT_CONTROL_FD, 1 << 4);
        assert_eq!(CAPTURE_CAPS_MASK & CAPTURE_CAP_GRANT_CONTROL_FD, 1 << 4);
        assert_eq!(std::mem::size_of::<DrmCastkmsGetGrant>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsGetGrant>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsGetGrant, output_index), 20);
    }

    #[test]
    fn renderer_operations_match_the_uapi_layouts() {
        assert_eq!(RENDERER_VERSION, 8);
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
        assert_eq!(std::mem::size_of::<DrmCastkmsRendererDequeueScene>(), 24);
        assert_eq!(std::mem::align_of::<DrmCastkmsRendererDequeueScene>(), 8);
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
    fn audio_tap_matches_the_uapi_layout() {
        assert_eq!(GRANT_CAPTURE_AUDIO, 1 << 5);
        assert_eq!(AUDIO_FORMAT_S16_LE, 1);
        assert_eq!(std::mem::size_of::<DrmCastkmsOpenAudioTap>(), 48);
        assert_eq!(std::mem::align_of::<DrmCastkmsOpenAudioTap>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsOpenAudioTap, fd), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsOpenAudioTap, buffer_frames),
            32
        );
    }

    #[test]
    fn grant_creation_matches_the_uapi_layout() {
        assert_eq!(GRANT_CREATE_FLAGS_MASK, 0b11);
        assert_eq!(std::mem::size_of::<DrmCastkmsCreateGrant>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsCreateGrant>(), 4);
        assert_eq!(std::mem::offset_of!(DrmCastkmsCreateGrant, fd), 12);
        assert_eq!(std::mem::offset_of!(DrmCastkmsCreateGrant, control_fd), 24);
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
        assert_eq!(std::mem::size_of::<DrmCastkmsGetOutput>(), 16);
        assert_eq!(std::mem::align_of::<DrmCastkmsGetOutput>(), 4);
        assert_eq!(std::mem::offset_of!(DrmCastkmsGetOutput, output_index), 8);
    }

    #[test]
    fn attachment_operations_match_the_uapi_layouts() {
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureSetOutputEdid>(), 24);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureSetOutputEdid>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureAttachMonitor>(), 40);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureAttachMonitor>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsCaptureAttachMonitor, display_name_ptr),
            24
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureDetachMonitor>(), 16);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureDetachMonitor>(), 8);
    }

    #[test]
    fn cec_operations_and_event_match_the_uapi_layouts() {
        assert_eq!(std::mem::size_of::<DrmCastkmsCecQueryCaps>(), 40);
        assert_eq!(std::mem::align_of::<DrmCastkmsCecQueryCaps>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsCecBindTransport>(), 48);
        assert_eq!(std::mem::align_of::<DrmCastkmsCecBindTransport>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsCecBindTransport, pad0), 44);
        assert_eq!(std::mem::size_of::<DrmCastkmsCecUnbindTransport>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsCecSetTransportState>(), 16);
        assert_eq!(std::mem::size_of::<DrmCastkmsCecTxComplete>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsCecTxComplete>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsCecReceive>(), 40);
        assert_eq!(std::mem::align_of::<DrmCastkmsCecReceive>(), 8);
        assert_eq!(std::mem::offset_of!(DrmCastkmsCecReceive, pad0), 35);
        assert_eq!(std::mem::size_of::<DrmCastkmsCecGetState>(), 112);
        assert_eq!(std::mem::align_of::<DrmCastkmsCecGetState>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsCecGetState, pending_cookie),
            48
        );
        assert_eq!(std::mem::size_of::<DrmEventCastkmsCecTx>(), 72);
        assert_eq!(std::mem::align_of::<DrmEventCastkmsCecTx>(), 8);
        assert_eq!(std::mem::offset_of!(DrmEventCastkmsCecTx, msg), 50);
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCecTx, signal_free_time),
            68
        );
    }

    #[test]
    fn capture_operations_match_the_uapi_layouts() {
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureFormat>(), 16);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureFormat>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureQueryCaps>(), 40);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureQueryCaps>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsCaptureQueryCaps, formats_ptr),
            24
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureStart>(), 24);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureStart>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsCaptureStart, mode_generation),
            16
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureStop>(), 16);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureStop>(), 8);
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureRegisterBuffer>(), 32);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureRegisterBuffer>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsCaptureRegisterBuffer, mode_generation),
            24
        );
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureUnregisterBuffer>(), 16);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureUnregisterBuffer>(), 4);
        assert_eq!(std::mem::size_of::<DrmCastkmsCaptureQueueBuffer>(), 48);
        assert_eq!(std::mem::align_of::<DrmCastkmsCaptureQueueBuffer>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmCastkmsCaptureQueueBuffer, user_data),
            16
        );
    }

    #[test]
    fn standard_drm_buffer_operations_match_the_uapi_layouts() {
        assert_eq!(std::mem::size_of::<DrmModeModeInfo>(), 68);
        assert_eq!(std::mem::align_of::<DrmModeModeInfo>(), 4);
        assert_eq!(std::mem::offset_of!(DrmModeModeInfo, vrefresh), 24);
        assert_eq!(std::mem::offset_of!(DrmModeModeInfo, name), 36);
        assert_eq!(std::mem::size_of::<DrmModeCrtc>(), 104);
        assert_eq!(std::mem::align_of::<DrmModeCrtc>(), 8);
        assert_eq!(std::mem::offset_of!(DrmModeCrtc, mode), 36);
        assert_eq!(std::mem::size_of::<DrmModeCreateDumb>(), 32);
        assert_eq!(std::mem::align_of::<DrmModeCreateDumb>(), 8);
        assert_eq!(std::mem::offset_of!(DrmModeCreateDumb, size), 24);
        assert_eq!(std::mem::size_of::<DrmModeDestroyDumb>(), 4);
        assert_eq!(std::mem::size_of::<DrmModeFbCmd2>(), 104);
        assert_eq!(std::mem::align_of::<DrmModeFbCmd2>(), 8);
        assert_eq!(std::mem::offset_of!(DrmModeFbCmd2, modifier), 72);
        assert_eq!(std::mem::size_of::<DrmPrimeHandle>(), 12);
        assert_eq!(std::mem::align_of::<DrmPrimeHandle>(), 4);
        assert_eq!(std::mem::size_of::<DmaBufExportSyncFile>(), 8);
        assert_eq!(std::mem::size_of::<DmaBufSync>(), 8);
        assert_eq!(std::mem::align_of::<DmaBufSync>(), 8);
        assert_eq!(std::mem::size_of::<DrmSyncobjCreate>(), 8);
        assert_eq!(std::mem::align_of::<DrmSyncobjCreate>(), 4);
        assert_eq!(std::mem::size_of::<DrmSyncobjDestroy>(), 8);
        assert_eq!(std::mem::align_of::<DrmSyncobjDestroy>(), 4);
        assert_eq!(std::mem::size_of::<DrmSyncobjHandle>(), 24);
        assert_eq!(std::mem::align_of::<DrmSyncobjHandle>(), 8);
        assert_eq!(std::mem::offset_of!(DrmSyncobjHandle, fd), 8);
        assert_eq!(std::mem::offset_of!(DrmSyncobjHandle, point), 16);
        assert_eq!(std::mem::size_of::<DrmSyncobjTimelineArray>(), 24);
        assert_eq!(std::mem::align_of::<DrmSyncobjTimelineArray>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmSyncobjTimelineArray, count_handles),
            16
        );
        assert_eq!(std::mem::size_of::<DrmSyncobjEventfd>(), 24);
        assert_eq!(std::mem::align_of::<DrmSyncobjEventfd>(), 8);
        assert_eq!(std::mem::offset_of!(DrmSyncobjEventfd, point), 8);
        assert_eq!(std::mem::offset_of!(DrmSyncobjEventfd, fd), 16);
    }

    #[test]
    fn events_match_the_uapi_layouts() {
        assert_eq!(std::mem::size_of::<DrmEvent>(), 8);
        assert_eq!(std::mem::align_of::<DrmEvent>(), 4);

        assert_eq!(std::mem::size_of::<DrmEventCastkmsGrantRevoked>(), 24);
        assert_eq!(std::mem::align_of::<DrmEventCastkmsGrantRevoked>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsGrantRevoked, timestamp_ns),
            16
        );

        assert_eq!(std::mem::size_of::<DrmEventCastkmsGrantState>(), 32);
        assert_eq!(std::mem::align_of::<DrmEventCastkmsGrantState>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsGrantState, timestamp_ns),
            24
        );

        assert_eq!(std::mem::size_of::<DrmEventCastkmsCaptureFrame>(), 112);
        assert_eq!(std::mem::align_of::<DrmEventCastkmsCaptureFrame>(), 8);
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCaptureFrame, user_data),
            8
        );
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCaptureFrame, mode_generation),
            32
        );
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCaptureFrame, stream_id),
            40
        );
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCaptureFrame, damage_x),
            60
        );
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCaptureFrame, cursor_serial),
            76
        );
        assert_eq!(
            std::mem::offset_of!(DrmEventCastkmsCaptureFrame, reserved),
            108
        );
    }
}
