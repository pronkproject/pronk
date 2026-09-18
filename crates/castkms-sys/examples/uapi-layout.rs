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
    layout!(
        DrmCastkmsRendererConfigure,
        "drm_castkms_renderer_configure"
    );
    layout!(DrmCastkmsRendererPublish, "drm_castkms_renderer_publish");
    layout!(
        DrmCastkmsRendererPublishResult,
        "drm_castkms_renderer_publish_result"
    );
    layout!(DrmCastkmsRendererWithdraw, "drm_castkms_renderer_withdraw");
    layout!(
        DrmCastkmsRendererMemoryPlane,
        "drm_castkms_renderer_memory_plane"
    );
    layout!(
        DrmCastkmsRendererReleaseJob,
        "drm_castkms_renderer_release_job"
    );
    layout!(
        DrmCastkmsRendererAcquireOutput,
        "drm_castkms_renderer_acquire_output"
    );
    layout!(DrmCastkmsRendererOutput, "drm_castkms_renderer_output");
    layout!(
        DrmCastkmsRendererReleaseOutput,
        "drm_castkms_renderer_release_output"
    );
    layout!(
        DrmCastkmsRendererConstraints,
        "drm_castkms_renderer_constraints"
    );
    layout!(
        DrmCastkmsRendererConstraintsFormat,
        "drm_castkms_renderer_constraints_format"
    );
    layout!(
        DrmCastkmsRendererAcquireJob,
        "drm_castkms_renderer_acquire_job"
    );
    layout!(
        DrmCastkmsRendererRegisterImage,
        "drm_castkms_renderer_register_image"
    );
    layout!(
        DrmCastkmsRendererUnregisterImage,
        "drm_castkms_renderer_unregister_image"
    );
    field!(
        DrmCastkmsRendererConfigure,
        constraints,
        "drm_castkms_renderer_configure.constraints"
    );
    field!(
        DrmCastkmsRendererPublish,
        result,
        "drm_castkms_renderer_publish.result"
    );
    field!(
        DrmCastkmsRendererPublish,
        ready_fence_fd,
        "drm_castkms_renderer_publish.ready_fence_fd"
    );
    field!(
        DrmCastkmsRendererAcquireJob,
        target_image_id,
        "drm_castkms_renderer_acquire_job.target_image_id"
    );
    field!(
        DrmCastkmsRendererAcquireOutput,
        image_id,
        "drm_castkms_renderer_acquire_output.image_id"
    );
    field!(
        DrmCastkmsRendererOutput,
        dma_buf_fd,
        "drm_castkms_renderer_output.dma_buf_fd"
    );
    field!(
        DrmCastkmsRendererOutput,
        offset,
        "drm_castkms_renderer_output.offset"
    );
    layout!(DrmCastkmsRendererJob, "drm_castkms_renderer_job");
    field!(
        DrmCastkmsRendererJob,
        constraints_id,
        "drm_castkms_renderer_job.constraints_id"
    );
    layout!(DrmCastkmsRendererPlane, "drm_castkms_renderer_plane");
    field!(
        DrmCastkmsRendererPlane,
        memory_planes,
        "drm_castkms_renderer_plane.memory_planes"
    );
    layout!(DrmCastkmsRendererColorOp, "drm_castkms_renderer_color_op");
}
