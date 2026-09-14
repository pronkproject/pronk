use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::AsRawFd;

use crate::Client;

const INVALID_MODIFIER: u64 = (1 << 56) - 1;

/// A configuration name valid only within the client that returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfferId(NonZeroU64);

impl OfferId {
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// An offered final-image layout, not a destination allocation description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Description {
    pub offer: OfferId,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub format: u32,
    pub modifier: u64,
    pub max_requests: NonZeroU32,
}

#[derive(Default)]
#[repr(C)]
struct Describe {
    id: u64,
    width: u32,
    height: u32,
    format: u32,
    max_requests: u32,
    modifier: u64,
    reserved: [u64; 2],
}

nix::ioctl_read!(describe, b'd', 0x00, Describe);

impl Client {
    /// Query current permission and the latest configuration without capturing.
    pub fn describe(&self) -> io::Result<Description> {
        let mut output = Describe::default();
        // SAFETY: The initialized output is writable for its complete ABI size.
        unsafe { describe(self.fd.as_raw_fd(), &mut output) }?;
        output.decode()
    }
}

impl Describe {
    fn decode(self) -> io::Result<Description> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid capture description");
        if self.reserved != [0; 2] || self.format == 0 || self.modifier == INVALID_MODIFIER {
            return Err(invalid());
        }
        Ok(Description {
            offer: OfferId(NonZeroU64::new(self.id).ok_or_else(invalid)?),
            width: NonZeroU32::new(self.width).ok_or_else(invalid)?,
            height: NonZeroU32::new(self.height).ok_or_else(invalid)?,
            format: self.format,
            modifier: self.modifier,
            max_requests: NonZeroU32::new(self.max_requests).ok_or_else(invalid)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    fn valid() -> Describe {
        Describe {
            id: 7,
            width: 640,
            height: 480,
            format: u32::from_le_bytes(*b"XR24"),
            max_requests: 4,
            ..Default::default()
        }
    }

    #[test]
    fn description_matches_the_kernel_layout() {
        assert_eq!(size_of::<Describe>(), 48);
        assert_eq!(offset_of!(Describe, modifier), 24);
        assert_eq!(offset_of!(Describe, reserved), 32);
        assert_eq!(nix::request_code_read!(b'd', 0, 48), 0x8030_6400);
    }

    #[test]
    fn description_retains_the_offer_without_inferring_allocation() {
        let output = valid().decode().unwrap();
        assert_eq!(output.offer.get(), 7);
        assert_eq!(output.max_requests.get(), 4);
        assert_eq!((output.width.get(), output.height.get()), (640, 480));
    }

    #[test]
    fn malformed_description_is_not_a_usable_offer() {
        for field in 0..7 {
            let mut raw = valid();
            match field {
                0 => raw.id = 0,
                1 => raw.width = 0,
                2 => raw.height = 0,
                3 => raw.format = 0,
                4 => raw.max_requests = 0,
                5 => raw.modifier = INVALID_MODIFIER,
                _ => raw.reserved[1] = 1,
            }
            assert_eq!(raw.decode().unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn unrelated_descriptor_is_rejected_and_closed() {
        let fd = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            Client::from_fd(fd.into()).unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
    }
}
