//! Opt-in allocation checks; no rendering, CPU pixel access or media transport.
#![cfg(feature = "vulkan")]

use std::num::NonZeroU32;
use std::os::fd::AsRawFd;

use pronk_gpu::output_pool::OutputPool;
use pronk_gpu::vulkan::Device;

fn selected() -> (Device, u64) {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
        .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
    let modifier = std::env::var("PRONK_GPU_MODIFIER")
        .expect("set PRONK_GPU_MODIFIER to an explicitly qualified hexadecimal modifier");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
    (
        Device::open(node).expect("open selected Vulkan device"),
        modifier,
    )
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn exported_images_retain_device_and_allocation_identity() {
    let (device, modifier) = selected();
    eprintln!("selected GPU: {}", device.name());
    let width = NonZeroU32::new(1920).unwrap();
    let height = NonZeroU32::new(1080).unwrap();
    let images: Vec<_> = (0..4)
        .map(|_| device.allocate(width, height, modifier).unwrap())
        .collect();
    // Images keep their native device and loader alive independently of the owner.
    drop(device);
    let mut buffers = Vec::new();
    for image in &images {
        let layout = image.layout();
        assert_eq!(
            (layout.width, layout.height, layout.modifier),
            (width, height, modifier)
        );
        assert!(layout.pitch > 0);
        assert!(layout.offset < layout.allocation_size);
        let first = image.export().unwrap();
        let alias = image.export().unwrap();
        let first_stat = nix::sys::stat::fstat(first.as_raw_fd()).unwrap();
        let alias_stat = nix::sys::stat::fstat(alias.as_raw_fd()).unwrap();
        assert_eq!(
            (first_stat.st_dev, first_stat.st_ino),
            (alias_stat.st_dev, alias_stat.st_ino)
        );
        assert!(
            nix::fcntl::fcntl(first.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFD).unwrap()
                & nix::libc::FD_CLOEXEC
                != 0
        );
        assert!(OutputPool::new(vec![first.try_clone().unwrap(), alias]).is_err());
        buffers.push(first);
    }
    let pool = OutputPool::new(buffers).unwrap();
    drop(images);
    // Exported storage owns its backing allocation after Vulkan image teardown.
    for slot in 0..4 {
        let fd = pool.export(slot).unwrap();
        assert!(nix::sys::stat::fstat(fd.as_raw_fd()).unwrap().st_size > 0);
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn unsupported_images_do_not_fall_back_to_another_layout() {
    let (device, modifier) = selected();
    let size = NonZeroU32::new(64).unwrap();
    assert!(device.allocate(size, size, u64::MAX).is_err());
    assert!(device
        .allocate(NonZeroU32::new(u32::MAX).unwrap(), size, modifier)
        .is_err());
    // Rejection leaves the device usable for a supported allocation.
    assert_eq!(
        device
            .allocate(size, size, modifier)
            .unwrap()
            .layout()
            .modifier,
        modifier
    );
}
