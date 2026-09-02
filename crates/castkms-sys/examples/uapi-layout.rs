// SPDX-License-Identifier: MIT

use castkms_sys::*;
use std::mem::{align_of, offset_of, size_of};

macro_rules! layout {
    ($rust:ty, $c:literal) => {
        println!(
            "layout {} {} {}",
            $c,
            size_of::<$rust>(),
            align_of::<$rust>()
        );
    };
}

macro_rules! field {
    ($rust:ty, $field:ident, $c:literal) => {
        println!("field {} {}", $c, offset_of!($rust, $field));
    };
}

fn main() {
    layout!(DrmCastkmsCaptureFormat, "drm_castkms_capture_format");
    layout!(DrmCastkmsCaptureQueryCaps, "drm_castkms_capture_query_caps");
    layout!(DrmCastkmsCaptureStart, "drm_castkms_capture_start");
    layout!(DrmCastkmsCaptureStop, "drm_castkms_capture_stop");
    layout!(
        DrmCastkmsCaptureRegisterBuffer,
        "drm_castkms_capture_register_buffer"
    );
    layout!(
        DrmCastkmsCaptureUnregisterBuffer,
        "drm_castkms_capture_unregister_buffer"
    );
    layout!(
        DrmCastkmsCaptureQueueBuffer,
        "drm_castkms_capture_queue_buffer"
    );
    layout!(
        DrmCastkmsCaptureSetOutputEdid,
        "drm_castkms_capture_set_output_edid"
    );
    layout!(
        DrmCastkmsCaptureAttachMonitor,
        "drm_castkms_capture_attach_monitor"
    );
    layout!(
        DrmCastkmsCaptureDetachMonitor,
        "drm_castkms_capture_detach_monitor"
    );
    layout!(DrmCastkmsCreateGrant, "drm_castkms_create_grant");
    layout!(DrmCastkmsGetGrant, "drm_castkms_get_grant");
    layout!(DrmCastkmsGetOutput, "drm_castkms_get_output");
    layout!(DrmCastkmsOpenAudioTap, "drm_castkms_open_audio_tap");
    layout!(
        DrmEventCastkmsGrantRevoked,
        "drm_event_castkms_grant_revoked"
    );
    layout!(DrmEventCastkmsGrantState, "drm_event_castkms_grant_state");
    layout!(
        DrmEventCastkmsCaptureFrame,
        "drm_event_castkms_capture_frame"
    );
    layout!(DrmCastkmsCecQueryCaps, "drm_castkms_cec_query_caps");
    layout!(DrmCastkmsCecBindTransport, "drm_castkms_cec_bind_transport");
    layout!(
        DrmCastkmsCecUnbindTransport,
        "drm_castkms_cec_unbind_transport"
    );
    layout!(
        DrmCastkmsCecSetTransportState,
        "drm_castkms_cec_set_transport_state"
    );
    layout!(DrmCastkmsCecTxComplete, "drm_castkms_cec_tx_complete");
    layout!(DrmCastkmsCecReceive, "drm_castkms_cec_receive");
    layout!(DrmCastkmsCecGetState, "drm_castkms_cec_get_state");
    layout!(DrmEventCastkmsCecTx, "drm_castkms_cec_event_tx");

    field!(
        DrmCastkmsCaptureQueryCaps,
        formats_ptr,
        "drm_castkms_capture_query_caps.formats_ptr"
    );
    field!(
        DrmCastkmsCaptureStart,
        mode_generation,
        "drm_castkms_capture_start.mode_generation"
    );
    field!(
        DrmCastkmsCaptureRegisterBuffer,
        mode_generation,
        "drm_castkms_capture_register_buffer.mode_generation"
    );
    field!(
        DrmCastkmsCaptureQueueBuffer,
        user_data,
        "drm_castkms_capture_queue_buffer.user_data"
    );
    field!(
        DrmCastkmsCaptureAttachMonitor,
        display_name_ptr,
        "drm_castkms_capture_attach_monitor.display_name_ptr"
    );
    field!(DrmCastkmsCreateGrant, fd, "drm_castkms_create_grant.fd");
    field!(
        DrmCastkmsCreateGrant,
        control_fd,
        "drm_castkms_create_grant.control_fd"
    );
    field!(
        DrmCastkmsGetGrant,
        output_index,
        "drm_castkms_get_grant.output_index"
    );
    field!(
        DrmCastkmsOpenAudioTap,
        buffer_frames,
        "drm_castkms_open_audio_tap.buffer_frames"
    );
    field!(
        DrmEventCastkmsCaptureFrame,
        cursor_serial,
        "drm_event_castkms_capture_frame.cursor_serial"
    );
    field!(
        DrmCastkmsCecGetState,
        pending_cookie,
        "drm_castkms_cec_get_state.pending_cookie"
    );
    field!(DrmEventCastkmsCecTx, msg, "drm_castkms_cec_event_tx.msg");
    field!(
        DrmEventCastkmsCecTx,
        signal_free_time,
        "drm_castkms_cec_event_tx.signal_free_time"
    );
}
