//! Async process and `SOCK_SEQPACKET` transport for one-shot helpers.

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::libc;
use nix::sys::socket::{
    recvmsg, sendmsg, socketpair, AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType,
    UnixAddr,
};
use thiserror::Error;
use tokio::io::{unix::AsyncFd, AsyncReadExt, Interest};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

const FIRST_NON_STDIO_FD: RawFd = 3;
const PROTOCOL_FD: RawFd = libc::STDOUT_FILENO;
const MAX_RECEIVED_FDS: usize = 2;
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub struct ReceivedDatagram {
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

#[derive(Debug)]
pub struct HelperExit {
    pub status: ExitStatus,
    pub diagnostics: Vec<u8>,
}

#[derive(Debug)]
pub struct HelperTransport {
    child: Child,
    socket: AsyncFd<OwnedFd>,
    diagnostics: JoinHandle<io::Result<Vec<u8>>>,
}

impl HelperTransport {
    pub fn spawn(mut command: Command) -> Result<Self, TransportError> {
        let (parent, child_endpoint) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )?;
        let parent = move_above_stdio(parent)?;
        let child_endpoint = move_above_stdio(child_endpoint)?;
        set_nonblocking(&parent)?;

        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child_endpoint_raw = child_endpoint.as_raw_fd();
        // SAFETY: The closure only calls async-signal-safe descriptor operations.
        // The endpoint remains open in the parent until spawn() has returned.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_endpoint_raw, PROTOCOL_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if child_endpoint_raw != PROTOCOL_FD && libc::close(child_endpoint_raw) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn()?;
        drop(child_endpoint);
        let stderr = child.stderr.take().ok_or(TransportError::MissingStderr)?;
        let diagnostics = tokio::spawn(read_bounded_diagnostics(stderr));
        let socket = AsyncFd::new(parent)?;

        Ok(Self {
            child,
            socket,
            diagnostics,
        })
    }

    pub fn child_id(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn send(&self, payload: &[u8]) -> Result<(), TransportError> {
        let sent = self
            .socket
            .async_io(Interest::WRITABLE, |socket| {
                let iov = [IoSlice::new(payload)];
                sendmsg::<UnixAddr>(
                    socket.as_raw_fd(),
                    &iov,
                    &[],
                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                    None,
                )
                .map_err(errno_to_io)
            })
            .await?;
        if sent != payload.len() {
            return Err(TransportError::ShortSend {
                expected: payload.len(),
                actual: sent,
            });
        }
        Ok(())
    }

    pub async fn receive(
        &self,
        maximum_payload_length: usize,
        maximum_fd_count: usize,
    ) -> Result<ReceivedDatagram, TransportError> {
        if maximum_payload_length == 0 {
            return Err(TransportError::InvalidPayloadLimit);
        }
        if maximum_fd_count > MAX_RECEIVED_FDS {
            return Err(TransportError::InvalidFileDescriptorLimit {
                maximum: MAX_RECEIVED_FDS,
                requested: maximum_fd_count,
            });
        }

        let mut payload = vec![0_u8; maximum_payload_length];
        self.socket
            .async_io(Interest::READABLE, |socket| {
                receive_once(socket.as_raw_fd(), &mut payload, maximum_fd_count)
            })
            .await
            .map_err(TransportError::Process)?
    }

    pub async fn finish(mut self, wait_timeout: Duration) -> Result<HelperExit, TransportError> {
        drop(self.socket);

        let status = match tokio::time::timeout(wait_timeout, self.child.wait()).await {
            Ok(result) => result?,
            Err(_) => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                self.diagnostics.abort();
                return Err(TransportError::ExitTimeout(wait_timeout));
            }
        };

        let diagnostics = match tokio::time::timeout(wait_timeout, &mut self.diagnostics).await {
            Ok(joined) => joined.map_err(TransportError::DiagnosticTask)??,
            Err(_) => {
                self.diagnostics.abort();
                return Err(TransportError::DiagnosticTimeout(wait_timeout));
            }
        };

        Ok(HelperExit {
            status,
            diagnostics,
        })
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Unix descriptor operation failed: {0}")]
    Descriptor(#[from] Errno),
    #[error("helper process operation failed: {0}")]
    Process(#[from] io::Error),
    #[error("spawned helper did not provide a diagnostic pipe")]
    MissingStderr,
    #[error("maximum payload length must be nonzero")]
    InvalidPayloadLimit,
    #[error("requested space for {requested} fds, maximum is {maximum}")]
    InvalidFileDescriptorLimit { maximum: usize, requested: usize },
    #[error("helper protocol socket reached EOF")]
    UnexpectedEof,
    #[error("helper datagram payload was truncated")]
    PayloadTruncated,
    #[error("helper datagram ancillary data was truncated")]
    AncillaryDataTruncated,
    #[error("helper sent an unexpected ancillary message")]
    UnexpectedControlMessage,
    #[error("helper sent {actual} fds, maximum allowed is {maximum}")]
    TooManyFileDescriptors { maximum: usize, actual: usize },
    #[error("sent {actual} bytes of a {expected}-byte seqpacket")]
    ShortSend { expected: usize, actual: usize },
    #[error("helper did not exit within {0:?}")]
    ExitTimeout(Duration),
    #[error("helper diagnostic pipe did not close within {0:?}")]
    DiagnosticTimeout(Duration),
    #[error("helper diagnostic reader task failed: {0}")]
    DiagnosticTask(tokio::task::JoinError),
}

fn receive_once(
    fd: RawFd,
    payload: &mut [u8],
    maximum_fd_count: usize,
) -> Result<Result<ReceivedDatagram, TransportError>, io::Error> {
    let mut iov = [IoSliceMut::new(payload)];
    let mut control = nix::cmsg_space!([RawFd; MAX_RECEIVED_FDS]);
    let message = recvmsg::<UnixAddr>(
        fd,
        &mut iov,
        Some(&mut control),
        MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .map_err(errno_to_io)?;

    let length = message.bytes;
    let flags = message.flags;
    let control_messages = message.cmsgs().map_err(errno_to_io)?.collect::<Vec<_>>();
    let _ = message;

    let mut received_fds = Vec::new();
    let mut unexpected_control_message = false;
    for control_message in control_messages {
        match control_message {
            ControlMessageOwned::ScmRights(raw_fds) => {
                received_fds.extend(raw_fds.into_iter().map(|raw_fd| {
                    // SAFETY: recvmsg returned ownership of each SCM_RIGHTS descriptor.
                    unsafe { OwnedFd::from_raw_fd(raw_fd) }
                }));
            }
            _ => unexpected_control_message = true,
        }
    }

    if flags.contains(MsgFlags::MSG_TRUNC) {
        return Ok(Err(TransportError::PayloadTruncated));
    }
    if flags.contains(MsgFlags::MSG_CTRUNC) {
        return Ok(Err(TransportError::AncillaryDataTruncated));
    }
    if unexpected_control_message {
        return Ok(Err(TransportError::UnexpectedControlMessage));
    }
    if received_fds.len() > maximum_fd_count {
        return Ok(Err(TransportError::TooManyFileDescriptors {
            maximum: maximum_fd_count,
            actual: received_fds.len(),
        }));
    }
    if length == 0 {
        return Ok(Err(TransportError::UnexpectedEof));
    }

    Ok(Ok(ReceivedDatagram {
        payload: payload[..length].to_vec(),
        fds: received_fds,
    }))
}

fn move_above_stdio(fd: OwnedFd) -> Result<OwnedFd, Errno> {
    if fd.as_raw_fd() >= FIRST_NON_STDIO_FD {
        return Ok(fd);
    }
    let duplicated = fcntl(
        fd.as_raw_fd(),
        FcntlArg::F_DUPFD_CLOEXEC(FIRST_NON_STDIO_FD),
    )?;
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn set_nonblocking(fd: &OwnedFd) -> Result<(), Errno> {
    let raw_flags = fcntl(fd.as_raw_fd(), FcntlArg::F_GETFL)?;
    let flags = OFlag::from_bits_truncate(raw_flags);
    fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

    let raw_fd_flags = fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD)?;
    let fd_flags = FdFlag::from_bits_truncate(raw_fd_flags);
    if !fd_flags.contains(FdFlag::FD_CLOEXEC) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

async fn read_bounded_diagnostics(mut stderr: tokio::process::ChildStderr) -> io::Result<Vec<u8>> {
    let mut saved = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stderr.read(&mut buffer).await?;
        if count == 0 {
            return Ok(saved);
        }
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(saved.len());
        saved.extend_from_slice(&buffer[..count.min(remaining)]);
    }
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

pub fn describe_exit(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        _ => "unknown process status".to_owned(),
    }
}
