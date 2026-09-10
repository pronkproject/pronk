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
    for _ in 0..4 {
        let device = Device::open(&node).expect("open selected Vulkan device");
        assert!(!device.name().is_empty());
    }
}
