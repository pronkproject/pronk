use super::*;
use crate::vulkan::PackedFormat;

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn rgb565_sources_keep_all_channel_levels_and_supply_opaque_alpha() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let mut original = producer
        .allocate_with_format(PackedFormat::Rgb565, nz(13), nz(7), modifier)
        .unwrap();
    let mut private = worker.allocate_private(nz(13), nz(7)).unwrap();
    for code in 0..64_u32 {
        let rgb = [code & 31, code, 31 - (code & 31)];
        let normalized = [
            (rgb[0] * 65535).div_ceil(31) as u16,
            (rgb[1] * 65535).div_ceil(63) as u16,
            (rgb[2] * 65535).div_ceil(31) as u16,
            0,
        ];
        let (written, producer) = original.clear_rgba16_and_wait(normalized).unwrap();
        let (written, raw) = readback(written);
        assert_eq!(raw.len(), 13 * 7 * 2);
        let expected = ((rgb[0] << 11) | (rgb[1] << 5) | rgb[2]) as u16;
        for pixel in raw.chunks_exact(2) {
            assert_eq!(u16::from_le_bytes(pixel.try_into().unwrap()), expected);
        }
        // SAFETY: Exact native RGB565 metadata and matching physical devices,
        // with completed foreign GENERAL release. The original is not reused
        // until its read into independent private storage completes.
        let source =
            unsafe { worker.import_source(written.export().unwrap(), written.layout(), producer) }
                .unwrap();
        private = source.copy_into_private_and_wait(private).unwrap();
        original = written.clear_and_wait([255; 3]).unwrap().0;
        let copied = private
            .copy_into_and_wait(worker.allocate(nz(13), nz(7), modifier).unwrap())
            .unwrap();
        private = copied.source;
        let (_, output) = readback(copied.destination);
        for pixel in output.chunks_exact(4) {
            for ((actual, channel), maximum) in pixel[..3]
                .iter()
                .zip([rgb[2], rgb[1], rgb[0]])
                .zip([31, 63, 31])
            {
                let scaled = channel * 255;
                assert!(
                    (scaled / maximum..=scaled.div_ceil(maximum)).contains(&u32::from(*actual)),
                    "RGB565 code {code}: {pixel:?}"
                );
            }
            assert_eq!(pixel[3], 255);
        }
    }
}
