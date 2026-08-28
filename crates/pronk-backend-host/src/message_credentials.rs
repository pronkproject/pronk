//! Unix transport which authenticates the writer of every D-Bus read.
//!
//! `SO_PEERCRED` describes the process at Unix-socket connection/listen time.
//! That is insufficient for the core side of a systemd `Accept=yes` socket:
//! systemd creates the listener, then transfers each accepted descriptor to a
//! backend process. `SO_PASSCRED` instead attaches kernel-authenticated
//! `SCM_CREDENTIALS` to each read, identifying the process which actually
//! wrote those bytes. This adapter strips that ancillary data before zbus sees
//! it, rejects a writer change, and exposes the authenticated identity through
//! zbus's ordinary peer-credentials API.

use std::future::poll_fn;
use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use async_trait::async_trait;
use nix::errno::Errno;
use nix::sys::socket::{
    recvmsg, sendmsg, setsockopt, sockopt, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr,
    UnixCredentials,
};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixSocket, UnixStream};
use zbus::connection::socket::{ReadHalf, Socket, Split, WriteHalf};
use zbus::fdo::ConnectionCredentials;

// The current backend-to-host protocol carries no descriptors. Retain a small
// bounded control buffer solely so unexpected SCM_RIGHTS can be received,
// closed with RAII, and rejected without allowing descriptor amplification.
const MAX_FDS_PER_READ: usize = 16;

#[derive(Debug)]
pub(crate) struct MessageCredentialsUnixStream {
    stream: UnixStream,
}

impl MessageCredentialsUnixStream {
    /// Connect with credential delivery enabled before the connection exists.
    ///
    /// Enabling `SO_PASSCRED` before `connect(2)` closes the race in which an
    /// activated backend could write its D-Bus authentication bytes before the
    /// option was installed.
    pub(crate) async fn connect(path: &Path) -> io::Result<Self> {
        let socket = UnixSocket::new_stream()?;
        enable_message_credentials(&socket)?;
        let stream = socket.connect(path).await?;
        Ok(Self { stream })
    }

    #[cfg(test)]
    fn from_connected(stream: UnixStream) -> io::Result<Self> {
        enable_message_credentials(&stream)?;
        Ok(Self { stream })
    }
}

fn enable_message_credentials(fd: &impl std::os::fd::AsFd) -> io::Result<()> {
    setsockopt(fd, sockopt::PassCred, &true).map_err(errno_to_io)
}

impl Socket for MessageCredentialsUnixStream {
    type ReadHalf = MessageCredentialsReadHalf;
    type WriteHalf = MessageCredentialsWriteHalf;

    fn split(self) -> Split<Self::ReadHalf, Self::WriteHalf> {
        let (read, write) = self.stream.into_split();
        let credentials = Arc::new(Mutex::new(None));
        Split::new(
            MessageCredentialsReadHalf {
                stream: read,
                credentials: credentials.clone(),
                prefetched_byte: None,
                prefetched_fds: Vec::new(),
            },
            MessageCredentialsWriteHalf {
                stream: write,
                credentials,
            },
        )
    }
}

#[derive(Debug)]
pub(crate) struct MessageCredentialsReadHalf {
    stream: OwnedReadHalf,
    credentials: Arc<Mutex<Option<UnixCredentials>>>,
    prefetched_byte: Option<u8>,
    prefetched_fds: Vec<OwnedFd>,
}

impl MessageCredentialsReadHalf {
    async fn receive_authenticated(
        &mut self,
        buffer: &mut [u8],
    ) -> io::Result<(usize, Vec<OwnedFd>)> {
        let stream = self.stream.as_ref();
        let received = poll_fn(|context| loop {
            match stream.try_io(tokio::io::Interest::READABLE, || {
                receive_with_credentials(stream.as_raw_fd(), buffer)
            }) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    match stream.poll_read_ready(context) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => result?,
                    }
                }
                result => return Poll::Ready(result),
            }
        })
        .await?;
        validate_writer(&self.credentials, received.credentials)?;
        Ok((received.bytes, received.fds))
    }

    async fn ensure_credentials(&mut self) -> io::Result<UnixCredentials> {
        if let Some(credentials) = *self
            .credentials
            .lock()
            .map_err(|_| poisoned_credentials())?
        {
            return Ok(credentials);
        }

        // zbus asks for the client credentials before it reads the EXTERNAL
        // authentication handshake. Read and retain its leading NUL byte so
        // the ordinary zbus handshake still consumes the complete stream.
        let mut byte = [0_u8; 1];
        let (bytes, fds) = self.receive_authenticated(&mut byte).await?;
        if bytes != 1 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "D-Bus peer closed before authentication",
            ));
        }
        self.prefetched_byte = Some(byte[0]);
        self.prefetched_fds = fds;
        (*self
            .credentials
            .lock()
            .map_err(|_| poisoned_credentials())?)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "D-Bus authentication read had no process credentials",
            )
        })
    }
}

#[async_trait]
impl ReadHalf for MessageCredentialsReadHalf {
    async fn recvmsg(&mut self, buffer: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
        if let Some(byte) = self.prefetched_byte.take() {
            let Some(first) = buffer.first_mut() else {
                self.prefetched_byte = Some(byte);
                return Ok((0, Vec::new()));
            };
            *first = byte;
            return Ok((1, std::mem::take(&mut self.prefetched_fds)));
        }
        self.receive_authenticated(buffer).await
    }

    fn can_pass_unix_fd(&self) -> bool {
        true
    }

    async fn peer_credentials(&mut self) -> io::Result<ConnectionCredentials> {
        connection_credentials(self.ensure_credentials().await?)
    }
}

#[derive(Debug)]
pub(crate) struct MessageCredentialsWriteHalf {
    stream: OwnedWriteHalf,
    credentials: Arc<Mutex<Option<UnixCredentials>>>,
}

#[async_trait]
impl WriteHalf for MessageCredentialsWriteHalf {
    async fn sendmsg(&mut self, buffer: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        let stream = self.stream.as_ref();
        poll_fn(|context| loop {
            match stream.try_io(tokio::io::Interest::WRITABLE, || {
                send_with_fds(stream.as_raw_fd(), buffer, fds)
            }) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    match stream.poll_write_ready(context) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => result?,
                    }
                }
                result => return Poll::Ready(result),
            }
        })
        .await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }

    fn can_pass_unix_fd(&self) -> bool {
        true
    }

    async fn peer_credentials(&mut self) -> io::Result<ConnectionCredentials> {
        let credentials = (*self
            .credentials
            .lock()
            .map_err(|_| poisoned_credentials())?)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "D-Bus peer credentials are not authenticated yet",
            )
        })?;
        connection_credentials(credentials)
    }
}

#[derive(Debug)]
struct CredentialedRead {
    bytes: usize,
    fds: Vec<OwnedFd>,
    credentials: UnixCredentials,
}

fn receive_with_credentials(fd: RawFd, buffer: &mut [u8]) -> io::Result<CredentialedRead> {
    let mut bytes = [IoSliceMut::new(buffer)];
    let mut control = nix::cmsg_space!(UnixCredentials, [RawFd; MAX_FDS_PER_READ]);
    let message = recvmsg::<UnixAddr>(
        fd,
        &mut bytes,
        Some(&mut control),
        MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .map_err(errno_to_io)?;
    let received_bytes = message.bytes;
    let flags = message.flags;
    let mut credentials = None;
    let mut fds = Vec::new();
    let mut unexpected_control = false;
    for control_message in message.cmsgs().map_err(errno_to_io)? {
        match control_message {
            ControlMessageOwned::ScmCredentials(value) => {
                if credentials.replace(value).is_some() {
                    unexpected_control = true;
                }
            }
            ControlMessageOwned::ScmRights(raw_fds) => {
                fds.extend(
                    raw_fds
                        .into_iter()
                        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                );
            }
            _ => unexpected_control = true,
        }
    }
    // Parse and own every received descriptor before rejecting the message.
    // Returning early on a malformed/truncated control layout would otherwise
    // leak any SCM_RIGHTS descriptors already installed by recvmsg(2).
    if received_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "D-Bus peer closed the Unix socket",
        ));
    }
    if flags.contains(MsgFlags::MSG_CTRUNC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "D-Bus ancillary data was truncated",
        ));
    }
    if unexpected_control {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "D-Bus read carried unexpected ancillary data",
        ));
    }
    if !fds.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "backend-to-host D-Bus traffic carried unexpected file descriptors",
        ));
    }
    let credentials = credentials.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "D-Bus read omitted SCM_CREDENTIALS",
        )
    })?;
    Ok(CredentialedRead {
        bytes: received_bytes,
        fds,
        credentials,
    })
}

fn validate_writer(
    established: &Mutex<Option<UnixCredentials>>,
    received: UnixCredentials,
) -> io::Result<()> {
    if received.pid() <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("D-Bus writer has invalid PID {}", received.pid()),
        ));
    }
    let mut established = established.lock().map_err(|_| poisoned_credentials())?;
    match *established {
        Some(expected) if expected != received => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "D-Bus writer changed from pid {}/uid {} to pid {}/uid {}",
                expected.pid(),
                expected.uid(),
                received.pid(),
                received.uid()
            ),
        )),
        Some(_) => Ok(()),
        None => {
            *established = Some(received);
            Ok(())
        }
    }
}

fn connection_credentials(credentials: UnixCredentials) -> io::Result<ConnectionCredentials> {
    let pid = u32::try_from(credentials.pid()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("D-Bus writer has invalid PID {}", credentials.pid()),
        )
    })?;
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "D-Bus writer has PID zero",
        ));
    }
    Ok(ConnectionCredentials::default()
        .set_process_id(pid)
        .set_unix_user_id(credentials.uid()))
}

fn poisoned_credentials() -> io::Error {
    io::Error::other("D-Bus peer credential state is poisoned")
}

fn send_with_fds(fd: RawFd, buffer: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
    let raw_fds: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let control = if raw_fds.is_empty() {
        Vec::new()
    } else {
        vec![ControlMessage::ScmRights(&raw_fds)]
    };
    let bytes = [IoSlice::new(buffer)];
    match sendmsg::<UnixAddr>(
        fd,
        &bytes,
        &control,
        MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
        None,
    ) {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write D-Bus message",
        )),
        Ok(bytes) => Ok(bytes),
        Err(error) => Err(errno_to_io(error)),
    }
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsFd;

    use nix::libc;
    use pronk_backend_protocol::{backend_host_builder, backend_peer_builder};

    use super::*;

    #[tokio::test]
    async fn supplies_the_actual_dbus_writer_to_zbus() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = backend_host_builder(
            MessageCredentialsUnixStream::from_connected(server_stream).unwrap(),
        )
        .unwrap();
        let client = backend_peer_builder(client_stream);
        let (server, client) = tokio::try_join!(server.build(), client.build()).unwrap();

        let credentials = server.peer_credentials().await.unwrap();
        assert_eq!(credentials.process_id(), Some(std::process::id()));
        assert_eq!(
            credentials.unix_user_id(),
            Some(nix::unistd::Uid::current().as_raw())
        );
        drop(client);
    }

    #[test]
    fn rejects_a_writer_change_between_dbus_reads() {
        let current = UnixCredentials::new();
        let changed = UnixCredentials::from(libc::ucred {
            pid: current.pid() + 1,
            uid: current.uid(),
            gid: current.gid(),
        });
        let established = Mutex::new(None);
        validate_writer(&established, current).unwrap();
        let error = validate_writer(&established, changed).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn rejects_and_closes_backend_to_host_descriptors() {
        let (server, client) = UnixStream::pair().unwrap();
        enable_message_credentials(&server).unwrap();
        let descriptor = File::open("/dev/null").unwrap();
        send_with_fds(client.as_raw_fd(), b"x", &[descriptor.as_fd()]).unwrap();

        let mut byte = [0_u8; 1];
        let error = receive_with_credentials(server.as_raw_fd(), &mut byte).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("unexpected file descriptors"));
    }
}
