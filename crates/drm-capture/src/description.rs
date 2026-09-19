use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

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
    pub refresh_millihz: NonZeroU32,
    pub mode_flags: u32,
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
    refresh_millihz: u32,
    mode_flags: u32,
    format: u32,
    max_requests: u32,
    modifier: u64,
    reserved: u64,
}

nix::ioctl_readwrite!(describe, b'd', 0x00, Describe);

/// One exact final-image layout requested from a capture provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedLayout {
    format: u32,
    modifier: u64,
}

impl RequestedLayout {
    pub fn new(format: u32, modifier: u64) -> io::Result<Self> {
        if format == 0 || modifier == INVALID_MODIFIER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid capture layout",
            ));
        }
        Ok(Self { format, modifier })
    }
}

impl<F: AsFd> Client<F> {
    /// Query current permission and the latest configuration without capturing.
    pub fn describe(&self) -> io::Result<Description> {
        query(self.as_fd())
    }

    pub fn describe_layout(&self, layout: RequestedLayout) -> io::Result<Description> {
        query_layout(self.as_fd(), Some(layout))
    }
}

pub(crate) fn query(fd: BorrowedFd<'_>) -> io::Result<Description> {
    query_layout(fd, None)
}

pub(crate) fn query_layout(
    fd: BorrowedFd<'_>,
    requested: Option<RequestedLayout>,
) -> io::Result<Description> {
    let mut output = Describe::default();
    if let Some(layout) = requested {
        output.format = layout.format;
        output.modifier = layout.modifier;
    }
    // SAFETY: The initialized output is writable for its complete ABI size,
    // and the borrowed file remains live throughout the query.
    unsafe { describe(fd.as_raw_fd(), &mut output) }?;
    output.decode_requested(requested)
}

impl Describe {
    fn decode_requested(self, requested: Option<RequestedLayout>) -> io::Result<Description> {
        let description = self.decode()?;
        if requested.is_some_and(|layout| {
            description.format != layout.format || description.modifier != layout.modifier
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture provider returned a different output layout",
            ));
        }
        Ok(description)
    }

    fn decode(self) -> io::Result<Description> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid capture description");
        if self.reserved != 0 || self.format == 0 || self.modifier == INVALID_MODIFIER {
            return Err(invalid());
        }
        Ok(Description {
            offer: OfferId(NonZeroU64::new(self.id).ok_or_else(invalid)?),
            width: NonZeroU32::new(self.width).ok_or_else(invalid)?,
            height: NonZeroU32::new(self.height).ok_or_else(invalid)?,
            refresh_millihz: NonZeroU32::new(self.refresh_millihz).ok_or_else(invalid)?,
            mode_flags: self.mode_flags,
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
            refresh_millihz: 60_000,
            format: u32::from_le_bytes(*b"XR24"),
            max_requests: 4,
            ..Default::default()
        }
    }

    #[test]
    fn description_matches_the_kernel_layout() {
        assert_eq!(size_of::<Describe>(), 48);
        assert_eq!(offset_of!(Describe, modifier), 32);
        assert_eq!(offset_of!(Describe, reserved), 40);
        assert_eq!(nix::request_code_readwrite!(b'd', 0, 48), 0xc030_6400);
    }

    #[test]
    fn description_retains_the_offer_without_inferring_allocation() {
        let output = valid().decode().unwrap();
        assert_eq!(output.offer.get(), 7);
        assert_eq!(output.max_requests.get(), 4);
        assert_eq!((output.width.get(), output.height.get()), (640, 480));
        assert_eq!(output.refresh_millihz.get(), 60_000);
    }

    #[test]
    fn exact_layout_excludes_the_default_request_and_invalid_modifier() {
        assert!(RequestedLayout::new(0, 0).is_err());
        assert!(RequestedLayout::new(u32::from_le_bytes(*b"AR24"), INVALID_MODIFIER).is_err());
        assert!(RequestedLayout::new(u32::from_le_bytes(*b"AR24"), 0).is_ok());
    }

    #[test]
    fn exact_request_rejects_a_mismatched_provider_description() {
        let requested = RequestedLayout::new(u32::from_le_bytes(*b"AR24"), 9).unwrap();
        assert_eq!(
            valid()
                .decode_requested(Some(requested))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let matching = Describe {
            format: u32::from_le_bytes(*b"AR24"),
            modifier: 9,
            ..valid()
        };
        assert!(matching.decode_requested(Some(requested)).is_ok());
        assert!(valid().decode_requested(None).is_ok());
    }

    #[test]
    fn malformed_description_is_not_a_usable_offer() {
        for field in 0..8 {
            let mut raw = valid();
            match field {
                0 => raw.id = 0,
                1 => raw.width = 0,
                2 => raw.height = 0,
                3 => raw.format = 0,
                4 => raw.max_requests = 0,
                5 => raw.modifier = INVALID_MODIFIER,
                6 => raw.refresh_millihz = 0,
                _ => raw.reserved = 1,
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
