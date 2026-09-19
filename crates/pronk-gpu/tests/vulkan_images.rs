//! Opt-in allocation checks; no rendering, CPU pixel access or media transport.
#![cfg(feature = "vulkan")]

use std::num::NonZeroU32;
use std::os::fd::AsRawFd;

use pronk_dmabuf::Completion;
use pronk_gpu::output_pool::OutputPool;
use pronk_gpu::vulkan::{Device, PackedFormat};

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
#[ignore = "requires an explicitly selected Vulkan render node"]
fn output_modifier_discovery_checks_export_and_reimport() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
        .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
    let device = Device::open(node).expect("open selected Vulkan device");
    let width = NonZeroU32::new(2560).unwrap();
    let height = NonZeroU32::new(1440).unwrap();
    for format in [PackedFormat::Bgra8, PackedFormat::Rgba8] {
        let modifiers = device.output_modifiers(format, width, height).unwrap();
        let private = device
            .private_storage_modifiers(format, width, height)
            .unwrap();
        eprintln!("{:?} output modifiers: {modifiers:#x?}", format);
        assert!(modifiers.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(private.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(modifiers.iter().all(|modifier| private.contains(modifier)));
    }
}

#[test]
#[ignore = "requires an explicitly selected Vulkan render node"]
fn four_k_capture_image_reports_native_allocation_size() {
    let node = std::env::var_os("PRONK_GPU_RENDER_NODE")
        .expect("set PRONK_GPU_RENDER_NODE to the intended render node");
    let device = Device::open(node).expect("open selected Vulkan device");
    let width = NonZeroU32::new(3840).unwrap();
    let height = NonZeroU32::new(2160).unwrap();
    for format in [PackedFormat::Bgra8, PackedFormat::Rgba8] {
        for modifier in device.output_modifiers(format, width, height).unwrap() {
            let image = device
                .allocate_with_format(format, width, height, modifier)
                .unwrap();
            eprintln!(
                "{format:?} modifier {modifier:#x}: {} bytes",
                image.layout().allocation_size
            );
        }
    }
    let private = device.allocate_private(width, height).unwrap();
    eprintln!(
        "private 4K scene image: {} bytes",
        private.allocation_size()
    );
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn negotiated_packed_layout_can_cross_the_private_to_recipient_boundary() {
    let (device, modifier) = selected();
    let width = NonZeroU32::new(2560).unwrap();
    let height = NonZeroU32::new(1440).unwrap();
    for format in [PackedFormat::Bgra8, PackedFormat::Rgba8] {
        assert!(device
            .output_modifiers(format, width, height)
            .unwrap()
            .contains(&modifier));
        let private = device
            .allocate_with_format(format, width, height, modifier)
            .unwrap();
        let (private, completion) = private.clear_and_wait([20, 80, 180]).unwrap();
        assert_eq!(completion.wait_blocking().unwrap(), Completion::Success);
        let recipient = device
            .allocate_with_format(format, width, height, modifier)
            .unwrap();
        let layout = recipient.layout();
        let descriptor = recipient.export().unwrap();
        // SAFETY: This test owns the independent allocation exclusively and uses it
        // only for the recipient copy before waiting for native completion.
        let recipient = unsafe { device.import_destination(descriptor, layout) }.unwrap();
        let copy = recipient.copy_from_and_wait(private).unwrap();
        assert_eq!(
            copy.completion.wait_blocking().unwrap(),
            Completion::Success
        );
    }
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn source_profiles_can_be_checked_without_allocating_storage() {
    let (device, modifier) = selected();
    let width = NonZeroU32::new(1920).unwrap();
    let height = NonZeroU32::new(1080).unwrap();
    for format in [
        PackedFormat::Bgra8,
        PackedFormat::Rgba8,
        PackedFormat::Bgr10A2,
        PackedFormat::Rgb10A2,
        PackedFormat::Rgb565,
    ] {
        device
            .check_source_image(format, width, height, modifier)
            .unwrap();
    }
    assert!(device
        .check_source_image(PackedFormat::Bgra8, width, height, u64::MAX)
        .is_err());
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn source_modifier_discovery_returns_only_checked_layouts() {
    let (device, selected) = selected();
    let width = NonZeroU32::new(1920).unwrap();
    let height = NonZeroU32::new(1080).unwrap();
    let modifiers = device
        .source_modifiers(PackedFormat::Bgra8, width, height)
        .unwrap();

    assert!(modifiers.contains(&selected));
    assert!(modifiers.windows(2).all(|pair| pair[0] < pair[1]));
    for modifier in modifiers {
        device
            .check_source_image(PackedFormat::Bgra8, width, height, modifier)
            .unwrap();
    }
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
        assert_eq!(layout.format, PackedFormat::Bgra8);
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
