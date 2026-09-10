use super::*;
use drm_display_executor::scene::color::Lut;

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn gamma_matches_reference_for_every_byte_at_table_extremes() {
    let (device, modifier) = device();
    let mut image = device.allocate_private(nz(9), nz(17)).unwrap();
    let mut output = device.allocate(nz(9), nz(17), modifier).unwrap();
    for count in [1usize, 2, 3, 256, 4097, 65536] {
        let entries = (0..count)
            .map(|index| {
                let value = if count == 1 {
                    23456
                } else {
                    index * 65535 / (count - 1)
                };
                [
                    value as u16,
                    (65535 - value) as u16,
                    ((value * 37) % 65536) as u16,
                ]
            })
            .collect::<Vec<_>>();
        let reference = Lut::new(&entries).unwrap();
        let gamma = device.create_gamma(&entries).unwrap();
        for value in 0..=255u8 {
            let rgb = [value, value.wrapping_mul(71), 255 - value];
            image = image.clear_waited(rgb).unwrap();
            image = gamma.apply_waited(image).unwrap();
            let copy = image.copy_into_waited(output).unwrap();
            image = copy.source;
            let expected = reference
                .sample(rgb.map(|v| u16::from(v) * 257))
                .map(|value| ((u32::from(value) + 128) / 257) as u8);
            let (returned, pixels) = readback(copy.destination);
            for pixel in pixels.chunks_exact(4) {
                assert_eq!(
                    pixel,
                    &[expected[2], expected[1], expected[0], 255],
                    "table size {count}, input {rgb:?}"
                );
            }
            output = returned;
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn gamma_preserves_imported_alpha_after_original_destruction() {
    let (device, modifier) = device();
    let (producer, _) = self::device();
    assert_eq!(device.identity(), producer.identity());
    let gamma = device.create_gamma(&[[65535, 12345, 0]]).unwrap();
    for alpha in [0, 1, 127, 128, 254, 255] {
        let input = producer.allocate(nz(13), nz(7), modifier).unwrap();
        let (input, ready) = input.clear_rgba_waited([21, 47, 91, alpha]).unwrap();
        // SAFETY: Matching native device/driver, exact allocator metadata and
        // completed foreign GENERAL release. The source remains untouched until
        // the waited private read retires its import.
        let source =
            unsafe { device.import_source(input.export().unwrap(), input.layout(), ready) }
                .unwrap();
        let image = source
            .copy_into_private_waited(device.allocate_private(nz(13), nz(7)).unwrap())
            .unwrap();
        drop(input);
        let image = gamma.apply_waited(image).unwrap();
        let copy = image
            .copy_into_waited(device.allocate(nz(13), nz(7), modifier).unwrap())
            .unwrap();
        let (_, pixels) = readback(copy.destination);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[0, 48, 255, alpha]);
        }
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn gamma_rejects_invalid_tables_or_unready_foreign_images() {
    let (device, _) = device();
    for entries in [Vec::new(), vec![[0; 3]; 65537]] {
        assert_eq!(
            device.create_gamma(&entries).err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    let gamma = device.create_gamma(&[[0; 3], [65535; 3]]).unwrap();
    let image = device.allocate_private(nz(1), nz(1)).unwrap();
    assert_eq!(
        gamma.apply_waited(image).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
    let (other, _) = self::device();
    let image = other
        .allocate_private(nz(1), nz(1))
        .unwrap()
        .clear_waited([0; 3])
        .unwrap();
    assert_eq!(
        gamma.apply_waited(image).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn gamma_clones_keep_upload_alive_through_concurrent_images() {
    let (device, modifier) = device();
    let weak = Arc::downgrade(&device.inner);
    let entries = [[0, 65535, 12345], [65535, 0, 12345]];
    let gamma = device.create_gamma(&entries).unwrap();
    let jobs = (0..3)
        .map(|_| {
            (
                gamma.clone(),
                device.allocate_private(nz(9), nz(17)).unwrap(),
                device.allocate(nz(9), nz(17), modifier).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let start = Arc::new(std::sync::Barrier::new(jobs.len() + 1));
    let threads = jobs
        .into_iter()
        .map(|(gamma, mut image, mut output)| {
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                for value in [0, 1, 127, 128, 254, 255] {
                    image = image.clear_waited([value; 3]).unwrap();
                    image = gamma.apply_waited(image).unwrap();
                    let copy = image.copy_into_waited(output).unwrap();
                    image = copy.source;
                    let (returned, pixels) = readback(copy.destination);
                    for pixel in pixels.chunks_exact(4) {
                        assert_eq!(pixel, &[48, 255 - value, value, 255]);
                    }
                    output = returned;
                }
            })
        })
        .collect::<Vec<_>>();
    drop(gamma);
    drop(device);
    start.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(
        weak.upgrade().is_none(),
        "gamma program retained its device after final use"
    );
}
