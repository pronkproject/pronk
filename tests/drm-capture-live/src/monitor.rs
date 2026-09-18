//! Monitor identity for a live probe's requested display mode.

use pronk_core::edid::{build_cast_display_edid, CastDisplayEdidRequest, EdidMode};
use pronk_core::identity::PnpId;

pub fn edid(width: u32, height: u32, refresh_millihz: u32) -> anyhow::Result<Vec<u8>> {
    let requested = EdidMode::new(width, height, refresh_millihz)?;
    let mut modes = vec![requested];
    if (width, height, refresh_millihz) != (640, 480, 60_000) {
        modes.push(EdidMode::new(640, 480, 60_000)?);
    }
    let monitor = build_cast_display_edid(CastDisplayEdidRequest {
        pnp_id: PnpId::parse("PRK")?,
        manufacturer_name: Some("Pronk".into()),
        product_name: Some("capture-live-test".into()),
        display_name: Some("Pronk capture test".into()),
        backend_id: "live-test".into(),
        device_id: "capture-test".into(),
        modes,
        audio: false,
        cec_physical_address: None,
    })?;
    Ok(monitor.edid().as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    #[test]
    fn requested_modes_produce_complete_valid_edids() {
        for (width, height, refresh) in [
            (640, 480, 60_000),
            (3840, 2160, 30_000),
            (2560, 1440, 60_000),
        ] {
            let bytes = super::edid(width, height, refresh).unwrap();
            assert_eq!(bytes.len() % 128, 0);
            for block in bytes.chunks_exact(128) {
                assert_eq!(
                    block.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
                    0
                );
            }
        }
    }

    #[test]
    fn an_invalid_mode_is_not_replaced_with_a_default() {
        assert!(super::edid(0, 1440, 60_000).is_err());
        assert!(super::edid(2560, 0, 60_000).is_err());
        assert!(super::edid(2560, 1440, 0).is_err());
    }
}
