use drm_display_executor::scene::{
    blend::Blend,
    geometry::{Extent, SourceRect},
    transform::Transform,
};

use super::*;

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn shared_program_keeps_concurrent_image_bindings_independent() {
    let (device, modifier) = device();
    let device_lifetime = std::sync::Arc::downgrade(&device.inner);
    let blender = device.create_blender().unwrap();
    let jobs: Vec<_> = (0..3)
        .map(|index| {
            (
                index,
                device.allocate_private(nz(17), nz(11)).unwrap(),
                device.allocate_private(nz(17), nz(11)).unwrap(),
                device.allocate(nz(17), nz(11), modifier).unwrap(),
            )
        })
        .collect();
    drop(device);
    let start = std::sync::Barrier::new(jobs.len() + 1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(index, mut source, mut destination, mut output)| {
                let blender = blender.clone();
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    let extent = Extent::new(17, 11).unwrap();
                    let crop = SourceRect::new(extent, [0, 0], extent).unwrap();
                    for iteration in 0..16 {
                        let rgb = [17 + index * 40, 85 + iteration * 7, 204];
                        source = source.clear_and_wait(rgb).unwrap();
                        destination = destination.clear_and_wait([0; 3]).unwrap();
                        let result = blender
                            .blend_region_and_wait(
                                destination,
                                source,
                                crop,
                                [0, 0],
                                Transform::default(),
                                Blend::default(),
                            )
                            .unwrap();
                        source = result.source;
                        let copied = result.destination.copy_into_and_wait(output).unwrap();
                        destination = copied.source;
                        let (returned, pixels) = readback(copied.destination);
                        output = returned;
                        assert!(pixels
                            .chunks_exact(4)
                            .all(|pixel| pixel == [rgb[2], rgb[1], rgb[0], 255]));
                    }
                })
            })
            .collect();
        drop(blender);
        start.wait();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    assert!(device_lifetime.upgrade().is_none());
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn program_rejects_images_from_another_logical_device() {
    let (device, _) = device();
    let (other, _) = super::device();
    let blender = other.create_blender().unwrap();
    let source = device
        .allocate_private(nz(17), nz(11))
        .unwrap()
        .clear_and_wait([0; 3])
        .unwrap();
    let destination = device
        .allocate_private(nz(17), nz(11))
        .unwrap()
        .clear_and_wait([0; 3])
        .unwrap();
    let extent = Extent::new(17, 11).unwrap();
    let crop = SourceRect::new(extent, [0, 0], extent).unwrap();
    assert_eq!(
        blender
            .blend_region_and_wait(
                destination,
                source,
                crop,
                [0, 0],
                Transform::default(),
                Blend::default()
            )
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}
