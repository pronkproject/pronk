use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;

/// Completion of submitted work, separately from the validity of its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Success,
    /// The native work ended with a negative Linux error code.
    Failed(i32),
}

/// An owned descriptor validated as a Linux sync file.
///
/// Callers must supply completion for submitted work, not a promise to submit
/// later. Descriptor validation establishes its type, not submission policy.
#[derive(Debug)]
pub struct SyncFile(OwnedFd);

impl SyncFile {
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        status(fd.as_fd())?;
        Ok(Self(fd))
    }

    /// Query native completion without waiting or consuming the descriptor.
    ///
    /// `None` means submitted work is pending. Errors are not completion
    /// evidence, and failed completion does not establish valid pixels.
    pub fn completion(&self) -> io::Result<Option<Completion>> {
        status(self.0.as_fd())
    }

    /// Combine two submitted completion records into one sync file.
    ///
    /// The returned record completes after both inputs. The kernel may discard
    /// completed inputs, including their errors: check the original records to
    /// establish pixel validity. Merging does not consume or change either
    /// source record. It joins already submitted native work; it cannot stand
    /// in for work that userspace intends to submit later.
    pub fn merge(&self, other: &Self) -> io::Result<Self> {
        let mut data = MergeData {
            name: merge_name(),
            fd2: other.0.as_raw_fd(),
            fence: -1,
            flags: 0,
            pad: 0,
        };
        // SAFETY: Both descriptors were validated as sync files. The writable
        // UAPI structure lives through the ioctl and contains no pointers.
        unsafe { merge(self.0.as_raw_fd(), &mut data) }?;
        if data.fence < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync-file merge returned no descriptor",
            ));
        }
        // SAFETY: A successful merge returns one new descriptor owned by the
        // caller. Validation below closes it on every error path.
        Self::from_fd(unsafe { OwnedFd::from_raw_fd(data.fence) })
    }

    /// Wait without blocking a Tokio worker. Errors are not successful pixels.
    ///
    /// Dropping the future closes its descriptor; it does not cancel native
    /// work or authorize reuse of storage still accessed by that work.
    pub async fn wait(self) -> io::Result<Completion> {
        wait_with_status(self.0, status).await
    }

    /// Wait on a dedicated blocking worker, checking native completion status.
    ///
    /// Never call from a PipeWire loop or an asynchronous runtime worker. There
    /// is no timeout: abandoning the wait would not cancel the submitted work.
    pub fn wait_blocking(self) -> io::Result<Completion> {
        wait_blocking_with_status(self.0, status)
    }

    pub fn into_fd(self) -> OwnedFd {
        self.0
    }
}

impl AsFd for SyncFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[repr(C)]
#[derive(Default)]
struct FileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

#[repr(C)]
struct MergeData {
    name: [u8; 32],
    fd2: i32,
    fence: i32,
    flags: u32,
    pad: u32,
}

fn merge_name() -> [u8; 32] {
    let mut name = [0; 32];
    let label = b"pronk-source-reads";
    name[..label.len()].copy_from_slice(label);
    name
}

nix::ioctl_readwrite!(merge, b'>', 3, MergeData);
nix::ioctl_readwrite!(file_info, b'>', 4, FileInfo);

fn status(fd: BorrowedFd<'_>) -> io::Result<Option<Completion>> {
    let mut info = FileInfo::default();
    // SAFETY: The writable UAPI structure lives through the ioctl. A zero
    // fence count requests aggregate status without a userspace array.
    unsafe { file_info(fd.as_raw_fd(), &mut info) }?;
    decode_status(info.status)
}

fn decode_status(status: i32) -> io::Result<Option<Completion>> {
    match status {
        0 => Ok(None),
        1 => Ok(Some(Completion::Success)),
        error if error < 0 => Ok(Some(Completion::Failed(error))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sync-file status",
        )),
    }
}

async fn wait_with_status(
    fd: OwnedFd,
    mut query: impl FnMut(BorrowedFd<'_>) -> io::Result<Option<Completion>>,
) -> io::Result<Completion> {
    let fd = AsyncFd::new(fd)?;
    loop {
        if let Some(completion) = query(fd.get_ref().as_fd())? {
            return Ok(completion);
        }
        let mut readiness = fd.readable().await?;
        if let Some(completion) = query(fd.get_ref().as_fd())? {
            return Ok(completion);
        }
        readiness.clear_ready();
    }
}

fn wait_blocking_with_status(
    fd: OwnedFd,
    mut query: impl FnMut(BorrowedFd<'_>) -> io::Result<Option<Completion>>,
) -> io::Result<Completion> {
    loop {
        if let Some(completion) = query(fd.as_fd())? {
            return Ok(completion);
        }
        let mut poll = nix::libc::pollfd {
            fd: fd.as_raw_fd(),
            events: nix::libc::POLLIN,
            revents: 0,
        };
        // SAFETY: One initialized poll descriptor remains live through the call.
        let result = unsafe { nix::libc::poll(&mut poll, 1, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if let Some(completion) = query(fd.as_fd())? {
            return Ok(completion);
        }
        if poll.revents & (nix::libc::POLLERR | nix::libc::POLLHUP | nix::libc::POLLNVAL) != 0 {
            return Err(io::Error::other(
                "sync file polling failed without completion",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn blocking_wait_checks_completion_after_readiness() {
        for completion in [Completion::Success, Completion::Failed(-5)] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&[1]).unwrap();
            let mut queries = 0;
            assert_eq!(
                wait_blocking_with_status(reader.into(), |_| {
                    queries += 1;
                    Ok((queries == 2).then_some(completion))
                })
                .unwrap(),
                completion
            );
            assert_eq!(queries, 2);
        }
    }

    #[test]
    fn blocking_wait_rejects_query_errors_and_unexplained_hangup() {
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(writer);
        assert!(wait_blocking_with_status(reader.into(), |_| Ok(None)).is_err());
        let (reader, _writer) = UnixStream::pair().unwrap();
        assert!(
            wait_blocking_with_status(reader.into(), |_| Err(io::Error::other("query"))).is_err()
        );
    }

    #[test]
    fn uapi_layout() {
        assert_eq!(std::mem::size_of::<MergeData>(), 48);
        assert_eq!(std::mem::offset_of!(MergeData, fd2), 32);
        assert_eq!(std::mem::offset_of!(MergeData, fence), 36);
        assert_eq!(std::mem::offset_of!(MergeData, flags), 40);
        assert_eq!(std::mem::offset_of!(MergeData, pad), 44);
        assert_eq!(std::mem::size_of::<FileInfo>(), 56);
        assert_eq!(std::mem::offset_of!(FileInfo, sync_fence_info), 48);
    }

    #[test]
    fn completion_is_not_pixel_success() {
        assert_eq!(decode_status(0).unwrap(), None);
        assert_eq!(decode_status(1).unwrap(), Some(Completion::Success));
        assert_eq!(decode_status(-5).unwrap(), Some(Completion::Failed(-5)));
        assert!(decode_status(2).is_err());
    }

    #[test]
    fn readable_socket_is_not_a_sync_file() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[1]).unwrap();
        assert!(SyncFile::from_fd(reader.into()).is_err());
    }

    #[tokio::test]
    async fn wait_rechecks_native_status_after_readiness() {
        for completion in [Completion::Success, Completion::Failed(-5)] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            writer.write_all(&[1]).unwrap();
            let mut calls = 0;
            let result = wait_with_status(reader.into(), |_| {
                calls += 1;
                Ok((calls > 1).then_some(completion))
            })
            .await
            .unwrap();
            assert_eq!(result, completion);
            assert_eq!(calls, 2);
        }
    }

    #[tokio::test]
    async fn query_failure_is_not_completion() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let error = wait_with_status(reader.into(), |_| Err(io::Error::from_raw_os_error(5)))
            .await
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[tokio::test]
    async fn already_completed_fence_does_not_need_readiness() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let completion = wait_with_status(reader.into(), |_| Ok(Some(Completion::Failed(-5))))
            .await
            .unwrap();
        assert_eq!(completion, Completion::Failed(-5));
    }

    #[tokio::test]
    async fn pending_wait_yields_and_releases_its_descriptor_on_drop() {
        use std::future::{poll_fn, Future};
        use std::io::Read;
        use std::task::Poll;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.set_nonblocking(true).unwrap();
        let mut wait = Box::pin(wait_with_status(reader.into(), |_| Ok(None)));
        poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            writer.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(wait);
        assert_eq!(writer.read(&mut [0]).unwrap(), 0);
    }
}
