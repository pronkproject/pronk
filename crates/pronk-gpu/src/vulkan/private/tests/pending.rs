use std::num::NonZeroUsize;
use std::os::fd::AsFd;

use drm_display_executor::scheduler::source_use::{ClosedUse, SourceUse};
use pronk_dmabuf::SyncFile;

use super::*;

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn private_read_is_accounted_before_pixels_are_extracted() {
    let (producer, modifier) = device();
    let (worker, _) = device();
    assert_eq!(producer.identity(), worker.identity());
    let (source, ready) = producer
        .allocate(nz(31), nz(17), modifier)
        .unwrap()
        .clear_waited([17, 85, 204])
        .unwrap();
    // SAFETY: Exact matching native metadata and completed producer release;
    // the original is not reused until the pending read has completed.
    let imported =
        unsafe { worker.import_source(source.export().unwrap(), source.layout(), ready) }.unwrap();
    let owner = SourceUse::new(NonZeroUsize::new(1).unwrap()).unwrap();
    let permit = owner.begin().unwrap();
    owner.close();
    let pending = imported
        .submit_private_copy(worker.allocate_private(nz(31), nz(17)).unwrap())
        .unwrap();
    // None denotes an already-completed native read, not missing accounting.
    let record = pending
        .completion()
        .map(|sync| SyncFile::from_fd(sync.as_fd().try_clone_to_owned().unwrap()).unwrap());
    permit.submitted(record);
    let ClosedUse::Released(records) = owner.finish().unwrap() else {
        panic!("submitted private read did not close normally")
    };
    assert_eq!(records.len(), 1);
    let private = pending.wait().unwrap();
    for record in records.into_iter().flatten() {
        assert_eq!(record.completion().unwrap(), Some(Completion::Success));
    }
    drop(source.clear_waited([255; 3]).unwrap());
    let copied = private
        .copy_into_waited(worker.allocate(nz(31), nz(17), modifier).unwrap())
        .unwrap();
    let (_, pixels) = readback(copied.destination);
    assert!(pixels
        .chunks_exact(4)
        .all(|pixel| pixel == [204, 85, 17, 255]));
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn abandoned_private_read_retires_before_source_reuse() {
    let (device, modifier) = device();
    let (source, ready) = device
        .allocate(nz(31), nz(17), modifier)
        .unwrap()
        .clear_waited([17, 85, 204])
        .unwrap();
    // SAFETY: Exact local allocation metadata and native producer release.
    // Source reuse follows the pending owner's blocking retirement.
    let imported =
        unsafe { device.import_source(source.export().unwrap(), source.layout(), ready) }.unwrap();
    let pending = imported
        .submit_private_copy(device.allocate_private(nz(31), nz(17)).unwrap())
        .unwrap();
    let record = pending
        .completion()
        .map(|sync| SyncFile::from_fd(sync.as_fd().try_clone_to_owned().unwrap()).unwrap());
    drop(pending);
    if let Some(record) = record {
        assert_eq!(record.completion().unwrap(), Some(Completion::Success));
    }
    drop(source.clear_waited([255; 3]).unwrap());
}
