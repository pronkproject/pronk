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
        DrmCastkmsRendererPrepareOffer,
        "drm_castkms_renderer_prepare_offer"
    );
    layout!(
        DrmCastkmsRendererPublishOffer,
        "drm_castkms_renderer_publish_offer"
    );
    layout!(
        DrmCastkmsRendererOfferResult,
        "drm_castkms_renderer_offer_result"
    );
    layout!(
        DrmCastkmsRendererWithdrawOffer,
        "drm_castkms_renderer_withdraw_offer"
    );
    layout!(
        DrmCastkmsRendererSubmitProbe,
        "drm_castkms_renderer_submit_probe"
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
        DrmCastkmsRendererDequeueOutput,
        "drm_castkms_renderer_dequeue_output"
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
        DrmCastkmsRendererDequeueScene,
        "drm_castkms_renderer_dequeue_scene"
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
        DrmCastkmsRendererPrepareOffer,
        constraints,
        "drm_castkms_renderer_prepare_offer.constraints"
    );
    field!(
        DrmCastkmsRendererPublishOffer,
        result,
        "drm_castkms_renderer_publish_offer.result"
    );
    field!(
        DrmCastkmsRendererSubmitProbe,
        completion_fd,
        "drm_castkms_renderer_submit_probe.completion_fd"
    );
    field!(
        DrmCastkmsRendererDequeueScene,
        image_id,
        "drm_castkms_renderer_dequeue_scene.image_id"
    );
    field!(
        DrmCastkmsRendererDequeueOutput,
        image_id,
        "drm_castkms_renderer_dequeue_output.image_id"
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
    layout!(DrmCastkmsRendererScene, "drm_castkms_renderer_scene");
    field!(
        DrmCastkmsRendererScene,
        constraints_id,
        "drm_castkms_renderer_scene.constraints_id"
    );
}
