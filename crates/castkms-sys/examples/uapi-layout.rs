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
    layout!(DrmCastkmsRendererQuery, "drm_castkms_renderer_query");
    layout!(DrmCastkmsRendererTakeover, "drm_castkms_renderer_takeover");
    layout!(
        DrmCastkmsRendererBeginTakeover,
        "drm_castkms_renderer_begin_takeover"
    );
    layout!(
        DrmCastkmsRendererAbortTakeover,
        "drm_castkms_renderer_abort_takeover"
    );
    layout!(DrmCastkmsRendererSnapshot, "drm_castkms_renderer_snapshot");
    layout!(
        DrmCastkmsRendererGetSnapshot,
        "drm_castkms_renderer_get_snapshot"
    );
    layout!(
        DrmCastkmsRendererSubmitProbe,
        "drm_castkms_renderer_submit_probe"
    );
    layout!(
        DrmCastkmsRendererCommitTakeover,
        "drm_castkms_renderer_commit_takeover"
    );
    layout!(
        DrmCastkmsRendererSourcePlane,
        "drm_castkms_renderer_source_plane"
    );
    layout!(
        DrmCastkmsRendererReleaseSource,
        "drm_castkms_renderer_release_source"
    );
    layout!(
        DrmCastkmsCapabilityProfile,
        "drm_castkms_capability_profile"
    );
    layout!(DrmCastkmsCapabilityFormat, "drm_castkms_capability_format");
    layout!(
        DrmCastkmsRendererRegisterProfile,
        "drm_castkms_renderer_register_profile"
    );
    layout!(
        DrmCastkmsRendererProfileResult,
        "drm_castkms_renderer_profile_result"
    );
    layout!(
        DrmCastkmsRendererCapabilities,
        "drm_castkms_renderer_capabilities"
    );
    layout!(
        DrmCastkmsRendererQueryCapabilities,
        "drm_castkms_renderer_query_capabilities"
    );
    field!(
        DrmCastkmsRendererTakeover,
        execution_generation,
        "drm_castkms_renderer_takeover.execution_generation"
    );
    field!(
        DrmCastkmsRendererBeginTakeover,
        result,
        "drm_castkms_renderer_begin_takeover.result"
    );
    field!(
        DrmCastkmsRendererSnapshot,
        content_serial,
        "drm_castkms_renderer_snapshot.content_serial"
    );
    field!(
        DrmCastkmsRendererGetSnapshot,
        result,
        "drm_castkms_renderer_get_snapshot.result"
    );
    field!(
        DrmCastkmsRendererSubmitProbe,
        completion_fd,
        "drm_castkms_renderer_submit_probe.completion_fd"
    );
}
