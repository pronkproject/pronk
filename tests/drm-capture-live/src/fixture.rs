use std::ffi::{c_char, c_void, CString};
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

use anyhow::Context;

#[repr(C)]
struct Info {
    device: i32,
    source: i32,
    destination: i32,
    crtc: u32,
    connector: u32,
    stride: u32,
    width: u32,
    height: u32,
    size: u64,
}

extern "C" {
    fn capture_fixture_open(path: *const c_char) -> *mut c_void;
    fn capture_fixture_info(fixture: *const c_void) -> Info;
    fn capture_fixture_flip(fixture: *const c_void);
    fn capture_fixture_check_pixels(fixture: *const c_void, expected: u8);
    fn capture_fixture_close(fixture: *mut c_void);
}

/// Test-only KMS owner. Capture calls never enter the C fixture.
pub struct Fixture {
    raw: NonNull<c_void>,
    info: Info,
}

impl Fixture {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: The terminated path remains live; C allocates one owned fixture.
        let raw = NonNull::new(unsafe { capture_fixture_open(path.as_ptr()) })
            .context("create KMS fixture")?;
        // SAFETY: A successfully returned fixture is live and owned until Drop.
        let info = unsafe { capture_fixture_info(raw.as_ptr()) };
        Ok(Self { raw, info })
    }

    pub fn master(&self) -> BorrowedFd<'_> {
        // SAFETY: The fixture owns this descriptor until Drop; the borrow cannot outlive it.
        unsafe { BorrowedFd::borrow_raw(self.info.device) }
    }

    pub fn source(&self) -> BorrowedFd<'_> {
        // SAFETY: The fixture owns this exported source until Drop.
        unsafe { BorrowedFd::borrow_raw(self.info.source) }
    }

    pub fn destination(&self) -> BorrowedFd<'_> {
        // SAFETY: The fixture owns this exported destination until Drop.
        unsafe { BorrowedFd::borrow_raw(self.info.destination) }
    }

    pub fn crtc(&self) -> u32 {
        self.info.crtc
    }
    pub fn connector(&self) -> u32 {
        self.info.connector
    }
    pub fn stride(&self) -> u32 {
        self.info.stride
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }

    pub fn flip(&mut self) {
        // SAFETY: Exclusive access retains the fixture through its blocking modeset.
        unsafe { capture_fixture_flip(self.raw.as_ptr()) };
    }

    pub fn check_pixels(&self, expected: u8) {
        // SAFETY: The live fixture owns the mapped DMA-BUF for the complete check.
        unsafe { capture_fixture_check_pixels(self.raw.as_ptr(), expected) };
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: Exactly one owner releases the fixture and its native resources.
        unsafe { capture_fixture_close(self.raw.as_ptr()) };
    }
}
