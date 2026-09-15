use super::*;
use crate::vulkan::PackedFormat;

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn ten_bit_sources_preserve_precision_before_eight_bit_output() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    for format in [PackedFormat::Bgr10A2, PackedFormat::Rgb10A2] {
        let mut original = producer
            .allocate_with_format(format, nz(8), nz(3), modifier)
            .unwrap();
        let mut private = worker.allocate_private(nz(8), nz(3)).unwrap();
        for code in 0..=1023_u32 {
            let rgb = [code, 1023 - code, code ^ 0x155];
            let alpha = code & 3;
            let normalized = rgb.map(|channel| (channel * 65535).div_ceil(1023) as u16);
            let (written, producer) = original
                .clear_rgba16_waited([
                    normalized[0],
                    normalized[1],
                    normalized[2],
                    (alpha * 21845) as u16,
                ])
                .unwrap();
            let (written, raw) = readback(written);
            let expected_word = pack(format, rgb, alpha);
            for pixel in raw.chunks_exact(4) {
                assert_eq!(
                    u32::from_le_bytes(pixel.try_into().unwrap()),
                    expected_word,
                    "generated {format:?}, code {code}"
                );
            }
            // SAFETY: Exact ten-bit allocator metadata, matching physical GPU
            // and completed foreign GENERAL release. The original remains
            // unchanged until the private source read has retired.
            let source = unsafe {
                worker.import_source(written.export().unwrap(), written.layout(), producer)
            }
            .unwrap();
            private = source.copy_into_private_waited(private).unwrap();
            original = written.clear_waited([255; 3]).unwrap().0;

            let copied = private
                .copy_into_waited(
                    worker
                        .allocate_with_format(format, nz(8), nz(3), modifier)
                        .unwrap(),
                )
                .unwrap();
            private = copied.source;
            let (_, roundtrip) = readback(copied.destination);
            for pixel in roundtrip.chunks_exact(4) {
                let word = u32::from_le_bytes(pixel.try_into().unwrap());
                assert_eq!(
                    word, expected_word,
                    "private precision: {format:?}, code {code}"
                );
            }

            let copied = private
                .copy_into_waited(worker.allocate(nz(8), nz(3), modifier).unwrap())
                .unwrap();
            private = copied.source;
            let (_, output) = readback(copied.destination);
            for pixel in output.chunks_exact(4) {
                for (actual, channel) in pixel[..3].iter().zip([rgb[2], rgb[1], rgb[0]]) {
                    // Native UNORM conversion permits either neighboring code.
                    let scaled = channel * 255;
                    assert!(
                        (scaled / 1023..=scaled.div_ceil(1023)).contains(&u32::from(*actual)),
                        "eight-bit conversion: {format:?}, code {code}, pixel {pixel:?}"
                    );
                }
                assert_eq!(u32::from(pixel[3]), alpha * 85);
            }
        }
    }
}

fn pack(format: PackedFormat, rgb: [u32; 3], alpha: u32) -> u32 {
    let [red, green, blue] = rgb;
    let (low, high) = match format {
        PackedFormat::Bgr10A2 => (blue, red),
        PackedFormat::Rgb10A2 => (red, blue),
        _ => panic!("ten-bit fixture format"),
    };
    low | (green << 10) | (high << 20) | (alpha << 30)
}
