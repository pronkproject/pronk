use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

const VERSION: u32 = 1;
const EDID_BLOCK_SIZE: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub max_edid_size: usize,
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
    let max_edid_size = response.max_edid_size as usize;
    if response.version != VERSION
        || response.flags != 0
        || response.reserved != 0
        || max_edid_size < EDID_BLOCK_SIZE
        || max_edid_size % EDID_BLOCK_SIZE != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported CastKMS monitor-control contract",
        ));
    }
    Ok(Capabilities { max_edid_size })
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
