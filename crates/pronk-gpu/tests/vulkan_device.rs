//! Device selection is opt-in and never substitutes another GPU.
#![cfg(feature = "vulkan")]

use pronk_gpu::vulkan::Device;

#[test]
fn non_render_devices_are_rejected() {
    assert!(Device::open("/dev/null").is_err());
    assert!(Device::open("Cargo.toml").is_err());
}

#[test]
#[ignore = "requires explicit Vulkan GPU selection"]
fn selected_render_device_opens_repeatedly() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
        .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
    let expected = Device::open(&node).unwrap().identity();
    assert_ne!(expected.device, [0; 16]);
    assert_ne!(expected.driver, [0; 16]);
    for _ in 0..4 {
        let device = Device::open(&node).expect("open selected Vulkan device");
        assert!(!device.name().is_empty());
        assert_eq!(device.identity(), expected);
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU selection"]
fn logical_device_identity_survives_cloning_but_not_reopening() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
        .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
    let first = Device::open(&node).unwrap();
    let second = Device::open(&node).unwrap();
    assert_eq!(first.identity(), second.identity());
    assert!(!first.is_same_instance(&second));
    assert!(!second.is_same_instance(&first));
    let retained = first.clone();
    assert!(retained.is_same_instance(&first));
    drop(first);
    assert_eq!(retained.identity(), second.identity());
    assert!(!retained.is_same_instance(&second));
    let image = retained
        .allocate_private(31.try_into().unwrap(), 17.try_into().unwrap())
        .unwrap();
    drop(retained);
    image.clear_waited([17, 85, 204]).unwrap();
}
