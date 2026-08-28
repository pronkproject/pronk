use anyhow::{ensure, Context};
use pronk_core::edid::{
    build_cast_display_edid, CastDisplayEdidRequest, EdidMode, EDID_PRODUCT_NAME_MAX_BYTES,
};
use pronk_core::identity::PnpId;

mod edid_conformance;

use edid_conformance::ConformanceOutcome;

fn main() -> anyhow::Result<()> {
    let decoder = std::env::args_os()
        .nth(1)
        .context("usage: pronk-edid-upstream-test EDID-DECODE")?;
    ensure!(
        std::env::args_os().len() == 2,
        "usage: pronk-edid-upstream-test EDID-DECODE"
    );

    let normal_name = "Chromecast with Google TV Living Room";
    ensure!(
        check_fixture(&decoder, normal_name, false)? == ConformanceOutcome::Pass,
        "ordinary DisplayID fixture did not pass EDID conformance"
    );
    let maximum_name = "X".repeat(EDID_PRODUCT_NAME_MAX_BYTES);
    ensure!(
        check_fixture(&decoder, &maximum_name, false)? == ConformanceOutcome::Pass,
        "maximum DisplayID fixture did not pass EDID conformance"
    );
    ensure!(
        check_fixture(&decoder, normal_name, true)? == ConformanceOutcome::HdmiVsdbEdid14Exception,
        "CEC fixture did not report the pinned HDMI VSDB/EDID 1.4 exception"
    );

    println!("edid_displayid_long_product=pass");
    println!("edid_displayid_continuation=pass");
    println!("edid_no_base_product_name=pass");
    println!("edid_upstream_conformance=pass");
    println!("edid_cec_hdmi_revision_exception=pinned");
    Ok(())
}

fn check_fixture(
    decoder: &std::ffi::OsStr,
    product_name: &str,
    cec: bool,
) -> anyhow::Result<ConformanceOutcome> {
    let generated = build_cast_display_edid(CastDisplayEdidRequest {
        pnp_id: PnpId::parse("GGL")?,
        manufacturer_name: Some("Google, Inc.".into()),
        product_name: Some(product_name.into()),
        display_name: Some(product_name.into()),
        backend_id: "chromiacast".into(),
        device_id: "fixture-device-1234".into(),
        modes: vec![
            EdidMode::new(1920, 1080, 60_000)?,
            EdidMode::new(1280, 720, 60_000)?,
            EdidMode::new(640, 480, 60_000)?,
        ],
        audio: true,
        cec_physical_address: cec.then_some(0x1000),
    })?;

    edid_conformance::check(decoder, generated.edid().as_bytes(), product_name)
}
