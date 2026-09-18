//! Virtual-monitor control through an issued CastKMS capability.
//!
//! These operations borrow only the monitor-control descriptor. They confer no
//! capture, renderer, revocation, primary-node or compositor-session authority.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

const VERSION: u32 = 1;
const EDID_BLOCK_SIZE: usize = 128;
const CAP_CEC: u32 = 1 << 0;
const KNOWN_CAPABILITIES: u32 = CAP_CEC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub max_edid_size: usize,
    pub cec_transport: bool,
}

#[repr(C)]
#[derive(Default)]
struct Query {
    version: u32,
    flags: u32,
    max_edid_size: u32,
    reserved: u32,
}

#[repr(C)]
struct Attach {
    flags: u32,
    edid_size: u32,
    edid_ptr: u64,
}

#[repr(C)]
#[derive(Default)]
struct Detach {
    flags: u32,
    reserved: u32,
}

nix::ioctl_read!(query, b'd', 0x41, Query);
nix::ioctl_write_ptr!(attach, b'd', 0x42, Attach);
nix::ioctl_write_ptr!(detach, b'd', 0x43, Detach);

pub fn query_capabilities(fd: BorrowedFd<'_>) -> io::Result<Capabilities> {
    let mut response = Query::default();
    // SAFETY: The writable response has the exact fixed-width UAPI layout and
    // remains live for the synchronous ioctl.
    unsafe { query(fd.as_raw_fd(), &mut response) }?;
    Capabilities::try_from(response)
}

impl TryFrom<Query> for Capabilities {
    type Error = io::Error;

    fn try_from(response: Query) -> Result<Self, Self::Error> {
        let max_edid_size = response.max_edid_size as usize;
        if response.version != VERSION
            || response.flags & !KNOWN_CAPABILITIES != 0
            || response.reserved != 0
            || max_edid_size < EDID_BLOCK_SIZE
            || max_edid_size % EDID_BLOCK_SIZE != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "unsupported CastKMS monitor-control contract: version {}, flags {:#x}, maximum EDID size {}, reserved {:#x}",
                    response.version, response.flags, response.max_edid_size, response.reserved
                ),
            ));
        }
        Ok(Self {
            max_edid_size,
            cec_transport: response.flags & CAP_CEC != 0,
        })
    }
}

pub fn attach_monitor(fd: BorrowedFd<'_>, edid: Option<&[u8]>) -> io::Result<()> {
    let edid = edid.unwrap_or_default();
    if !edid.is_empty() && (edid.len() % EDID_BLOCK_SIZE != 0 || edid.len() > u32::MAX as usize) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid complete EDID size",
        ));
    }
    if edid.len() > query_capabilities(fd)?.max_edid_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "complete EDID exceeds the monitor-control limit",
        ));
    }
    let request = Attach {
        flags: 0,
        edid_size: edid.len() as u32,
        edid_ptr: if edid.is_empty() {
            0
        } else {
            edid.as_ptr() as u64
        },
    };
    // SAFETY: The request has the fixed-width UAPI layout. Its optional pointer
    // names `edid`, which remains live for the synchronous copying ioctl.
    unsafe { attach(fd.as_raw_fd(), &request) }?;
    Ok(())
}

pub fn detach_monitor(fd: BorrowedFd<'_>) -> io::Result<()> {
    let request = Detach::default();
    // SAFETY: The initialized request has the exact fixed-width UAPI layout and
    // remains live for the synchronous ioctl.
    unsafe { detach(fd.as_raw_fd(), &request) }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use std::os::fd::AsFd;

    #[test]
    fn requests_match_the_kernel_layout() {
        assert_eq!(VERSION, 1);
        assert_eq!(size_of::<Query>(), 16);
        assert_eq!(size_of::<Attach>(), 16);
        assert_eq!(offset_of!(Attach, edid_ptr), 8);
        assert_eq!(size_of::<Detach>(), 8);
        assert_eq!(nix::request_code_read!(b'd', 0x41, 16), 0x8010_6441);
        assert_eq!(nix::request_code_write!(b'd', 0x42, 16), 0x4010_6442);
        assert_eq!(nix::request_code_write!(b'd', 0x43, 8), 0x4008_6443);
    }

    #[test]
    fn accepts_known_optional_capabilities() {
        assert_eq!(
            Capabilities::try_from(Query {
                version: VERSION,
                flags: CAP_CEC,
                max_edid_size: 256,
                reserved: 0,
            })
            .unwrap(),
            Capabilities {
                max_edid_size: 256,
                cec_transport: true,
            }
        );
    }

    #[test]
    fn rejects_unknown_capabilities() {
        let error = Capabilities::try_from(Query {
            version: VERSION,
            flags: CAP_CEC << 1,
            max_edid_size: 256,
            reserved: 0,
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn ordinary_files_reject_monitor_operations() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            query_capabilities(file.as_fd()).unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        assert_eq!(
            attach_monitor(file.as_fd(), None)
                .unwrap_err()
                .raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
        assert_eq!(
            detach_monitor(file.as_fd()).unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTTY)
        );
    }

    #[test]
    fn partial_edids_are_rejected_before_the_ioctl() {
        let file = std::fs::File::open("/dev/null").unwrap();
        for edid in [vec![0; 127], vec![0; 129]] {
            assert_eq!(
                attach_monitor(file.as_fd(), Some(&edid))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
