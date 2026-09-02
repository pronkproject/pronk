//! Server-classified PipeWire native-protocol connection minting.
//!
//! These connections are deliberately raw. This module connects and inspects
//! Unix socket state, but never reads, writes, or passes a consumer connection
//! to libpipewire. The receiving backend must be the first protocol user.

use std::fmt;
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::sys::socket::{getpeername, getsockname, getsockopt, sockopt, SockType, UnixAddr};
use nix::unistd::Uid;
use thiserror::Error;
use tokio::net::UnixStream;

use crate::PipeWireRemote;

pub const PIPEWIRE_CORE_SOCKET_NAME: &str = "pipewire-0-pronk-core";
pub const PIPEWIRE_BACKEND_SOCKET_NAME: &str = "pipewire-0-pronk-backend";
const MAX_CONTEXT_STRING_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeWireConnectionRole {
    CoreProducer,
    BackendConsumer,
}

impl fmt::Display for PipeWireConnectionRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoreProducer => formatter.write_str("core producer"),
            Self::BackendConsumer => formatter.write_str("backend consumer"),
        }
    }
}

/// Absolute, distinct paths for the two server-classified native sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedSocketPaths {
    core: PathBuf,
    backend: PathBuf,
}

impl ClassifiedSocketPaths {
    pub fn new(
        core: impl Into<PathBuf>,
        backend: impl Into<PathBuf>,
    ) -> Result<Self, ClassifiedSocketPathsError> {
        let core = core.into();
        let backend = backend.into();
        validate_socket_path(PipeWireConnectionRole::CoreProducer, &core)?;
        validate_socket_path(PipeWireConnectionRole::BackendConsumer, &backend)?;
        if core == backend {
            return Err(ClassifiedSocketPathsError::SamePath);
        }
        Ok(Self { core, backend })
    }

    pub fn in_runtime_dir(
        runtime_dir: impl AsRef<Path>,
    ) -> Result<Self, ClassifiedSocketPathsError> {
        let runtime_dir = runtime_dir.as_ref();
        Self::new(
            runtime_dir.join(PIPEWIRE_CORE_SOCKET_NAME),
            runtime_dir.join(PIPEWIRE_BACKEND_SOCKET_NAME),
        )
    }

    pub fn core(&self) -> &Path {
        &self.core
    }

    pub fn backend(&self) -> &Path {
        &self.backend
    }
}

fn validate_socket_path(
    role: PipeWireConnectionRole,
    path: &Path,
) -> Result<(), ClassifiedSocketPathsError> {
    if !path.is_absolute() {
        return Err(ClassifiedSocketPathsError::NotAbsolute {
            role,
            path: path.to_owned(),
        });
    }
    UnixAddr::new(path).map_err(|source| ClassifiedSocketPathsError::InvalidUnixAddress {
        role,
        path: path.to_owned(),
        source,
    })?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ClassifiedSocketPathsError {
    #[error("{role} PipeWire socket path is not absolute: {path:?}")]
    NotAbsolute {
        role: PipeWireConnectionRole,
        path: PathBuf,
    },
    #[error(
        "{role} PipeWire socket path is not a valid pathname Unix address: {path:?}: {source}"
    )]
    InvalidUnixAddress {
        role: PipeWireConnectionRole,
        path: PathBuf,
        #[source]
        source: nix::Error,
    },
    #[error("core producer and backend consumer PipeWire socket paths must differ")]
    SamePath,
}

/// A connected producer-class fd that can only become a Pronk source remote.
#[derive(Debug)]
pub struct PipeWireProducerFd(OwnedFd);

impl PipeWireProducerFd {
    pub fn into_remote(self) -> PipeWireRemote {
        PipeWireRemote::Connected(self.0)
    }
}

impl AsFd for PipeWireProducerFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// One untouched consumer-class connection intended for fd transfer.
#[derive(Debug)]
pub struct PipeWireConsumerFd(OwnedFd);

impl PipeWireConsumerFd {
    pub fn into_owned_fd(self) -> OwnedFd {
        self.0
    }
}

impl AsFd for PipeWireConsumerFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Fresh consumer connections for one backend media generation.
#[derive(Debug)]
pub struct BackendRemoteSet {
    video: PipeWireConsumerFd,
    audio: Option<PipeWireConsumerFd>,
}

impl BackendRemoteSet {
    pub fn video(&self) -> &PipeWireConsumerFd {
        &self.video
    }

    pub fn audio(&self) -> Option<&PipeWireConsumerFd> {
        self.audio.as_ref()
    }

    pub fn into_parts(self) -> (PipeWireConsumerFd, Option<PipeWireConsumerFd>) {
        (self.video, self.audio)
    }
}

#[async_trait]
pub trait PipeWireRemoteProvider: Send + Sync {
    async fn create_backend_remotes(
        &self,
        session_id: &str,
        backend_id: &str,
        media_generation: NonZeroU64,
        needs_audio: bool,
    ) -> Result<BackendRemoteSet, RemoteProviderError>;
}

/// Initial remote provider backed by two packaged PipeWire native sockets.
#[derive(Debug, Clone)]
pub struct ClassifiedSocketRemoteProvider {
    paths: ClassifiedSocketPaths,
    expected_server_uid: Uid,
}

impl ClassifiedSocketRemoteProvider {
    pub fn new(paths: ClassifiedSocketPaths) -> Self {
        Self::new_for_server_uid(paths, Uid::effective())
    }

    /// Construct a provider for a PipeWire server owned by an explicitly
    /// selected account. System mode uses the dedicated `pronk` account;
    /// session mode defaults to the daemon's effective UID.
    pub fn new_for_server_uid(paths: ClassifiedSocketPaths, expected_server_uid: Uid) -> Self {
        Self {
            paths,
            expected_server_uid,
        }
    }

    pub fn paths(&self) -> &ClassifiedSocketPaths {
        &self.paths
    }

    /// Connect the core-class endpoint for one libpipewire-owned source.
    pub async fn create_producer_remote(&self) -> Result<PipeWireProducerFd, RemoteProviderError> {
        let fd = TokioUnixConnector
            .connect(
                PipeWireConnectionRole::CoreProducer,
                self.paths.core(),
                self.expected_server_uid,
            )
            .await?;
        Ok(PipeWireProducerFd(fd))
    }

    /// Mint fresh, distinct, untouched backend connections.
    pub async fn create_backend_remotes(
        &self,
        session_id: &str,
        backend_id: &str,
        media_generation: NonZeroU64,
        needs_audio: bool,
    ) -> Result<BackendRemoteSet, RemoteProviderError> {
        self.create_backend_remotes_with(
            &TokioUnixConnector,
            session_id,
            backend_id,
            media_generation,
            needs_audio,
        )
        .await
    }

    async fn create_backend_remotes_with<C: SocketConnector + ?Sized>(
        &self,
        connector: &C,
        session_id: &str,
        backend_id: &str,
        _media_generation: NonZeroU64,
        needs_audio: bool,
    ) -> Result<BackendRemoteSet, RemoteProviderError> {
        validate_context_string("session ID", session_id)?;
        validate_context_string("backend ID", backend_id)?;

        let video = PipeWireConsumerFd(
            connector
                .connect(
                    PipeWireConnectionRole::BackendConsumer,
                    self.paths.backend(),
                    self.expected_server_uid,
                )
                .await?,
        );
        let audio = if needs_audio {
            Some(PipeWireConsumerFd(
                connector
                    .connect(
                        PipeWireConnectionRole::BackendConsumer,
                        self.paths.backend(),
                        self.expected_server_uid,
                    )
                    .await?,
            ))
        } else {
            None
        };
        Ok(BackendRemoteSet { video, audio })
    }

    #[cfg(test)]
    fn with_expected_server_uid(paths: ClassifiedSocketPaths, expected_server_uid: Uid) -> Self {
        Self::new_for_server_uid(paths, expected_server_uid)
    }
}

#[async_trait]
impl PipeWireRemoteProvider for ClassifiedSocketRemoteProvider {
    async fn create_backend_remotes(
        &self,
        session_id: &str,
        backend_id: &str,
        media_generation: NonZeroU64,
        needs_audio: bool,
    ) -> Result<BackendRemoteSet, RemoteProviderError> {
        ClassifiedSocketRemoteProvider::create_backend_remotes(
            self,
            session_id,
            backend_id,
            media_generation,
            needs_audio,
        )
        .await
    }
}

fn validate_context_string(field: &'static str, value: &str) -> Result<(), RemoteProviderError> {
    if value.is_empty() || value.len() > MAX_CONTEXT_STRING_BYTES || value.contains('\0') {
        return Err(RemoteProviderError::InvalidContext { field });
    }
    Ok(())
}

#[async_trait]
trait SocketConnector: Send + Sync {
    /// Return a connected, validated fd without performing protocol I/O.
    async fn connect(
        &self,
        role: PipeWireConnectionRole,
        path: &Path,
        expected_server_uid: Uid,
    ) -> Result<OwnedFd, RemoteProviderError>;
}

struct TokioUnixConnector;

#[async_trait]
impl SocketConnector for TokioUnixConnector {
    async fn connect(
        &self,
        role: PipeWireConnectionRole,
        path: &Path,
        expected_server_uid: Uid,
    ) -> Result<OwnedFd, RemoteProviderError> {
        let stream =
            UnixStream::connect(path)
                .await
                .map_err(|source| RemoteProviderError::Connect {
                    role,
                    path: path.to_owned(),
                    source,
                })?;
        let stream = stream
            .into_std()
            .map_err(|source| RemoteProviderError::Connect {
                role,
                path: path.to_owned(),
                source,
            })?;
        let fd = OwnedFd::from(stream);
        validate_connected_socket(&fd, role, path, expected_server_uid)?;
        Ok(fd)
    }
}

fn validate_connected_socket(
    fd: &OwnedFd,
    role: PipeWireConnectionRole,
    path: &Path,
    expected_server_uid: Uid,
) -> Result<(), RemoteProviderError> {
    let socket_type = inspect(fd, role, path, "SO_TYPE", || {
        getsockopt(fd, sockopt::SockType)
    })?;
    if socket_type != SockType::Stream {
        return Err(RemoteProviderError::WrongSocketType {
            role,
            path: path.to_owned(),
            actual: socket_type,
        });
    }
    let accepting = inspect(fd, role, path, "SO_ACCEPTCONN", || {
        getsockopt(fd, sockopt::AcceptConn)
    })?;
    if accepting {
        return Err(RemoteProviderError::ListeningSocket {
            role,
            path: path.to_owned(),
        });
    }

    let raw_fd = fd.as_raw_fd();
    inspect(fd, role, path, "getsockname", || {
        getsockname::<UnixAddr>(raw_fd)
    })?;
    let peer = inspect(fd, role, path, "getpeername", || {
        getpeername::<UnixAddr>(raw_fd)
    })?;
    if peer.path() != Some(path) {
        return Err(RemoteProviderError::WrongPeerPath {
            role,
            expected: path.to_owned(),
            actual: peer.path().map(Path::to_owned),
        });
    }

    let flags = inspect(fd, role, path, "F_GETFD", || {
        fcntl(raw_fd, FcntlArg::F_GETFD).map(FdFlag::from_bits_truncate)
    })?;
    if !flags.contains(FdFlag::FD_CLOEXEC) {
        return Err(RemoteProviderError::MissingCloseOnExec {
            role,
            path: path.to_owned(),
        });
    }

    let credentials = inspect(fd, role, path, "SO_PEERCRED", || {
        getsockopt(fd, sockopt::PeerCredentials)
    })?;
    let actual_uid = Uid::from_raw(credentials.uid());
    if actual_uid != expected_server_uid {
        return Err(RemoteProviderError::WrongPeerUid {
            role,
            path: path.to_owned(),
            expected: expected_server_uid,
            actual: actual_uid,
        });
    }
    Ok(())
}

fn inspect<T>(
    _fd: &OwnedFd,
    role: PipeWireConnectionRole,
    path: &Path,
    operation: &'static str,
    inspect: impl FnOnce() -> Result<T, nix::Error>,
) -> Result<T, RemoteProviderError> {
    inspect().map_err(|source| RemoteProviderError::Inspect {
        role,
        path: path.to_owned(),
        operation,
        source,
    })
}

#[derive(Debug, Error)]
pub enum RemoteProviderError {
    #[error("{field} is empty, too long, or contains NUL")]
    InvalidContext { field: &'static str },
    #[error("connect {role} PipeWire socket {path:?}: {source}")]
    Connect {
        role: PipeWireConnectionRole,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("inspect {role} PipeWire socket {path:?} with {operation}: {source}")]
    Inspect {
        role: PipeWireConnectionRole,
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: nix::Error,
    },
    #[error("{role} PipeWire endpoint {path:?} is not SOCK_STREAM: {actual:?}")]
    WrongSocketType {
        role: PipeWireConnectionRole,
        path: PathBuf,
        actual: SockType,
    },
    #[error("{role} PipeWire endpoint {path:?} is a listening socket")]
    ListeningSocket {
        role: PipeWireConnectionRole,
        path: PathBuf,
    },
    #[error("{role} PipeWire endpoint lacks FD_CLOEXEC: {path:?}")]
    MissingCloseOnExec {
        role: PipeWireConnectionRole,
        path: PathBuf,
    },
    #[error(
        "{role} PipeWire endpoint peer path mismatch: expected {expected:?}, received {actual:?}"
    )]
    WrongPeerPath {
        role: PipeWireConnectionRole,
        expected: PathBuf,
        actual: Option<PathBuf>,
    },
    #[error(
        "{role} PipeWire endpoint {path:?} has peer uid {actual}, expected same-session uid {expected}"
    )]
    WrongPeerUid {
        role: PipeWireConnectionRole,
        path: PathBuf,
        expected: Uid,
        actual: Uid,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::io::{ErrorKind, Read};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::io::AsyncReadExt as _;
    use tokio::net::UnixListener;

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct SocketFixture {
        directory: PathBuf,
        paths: ClassifiedSocketPaths,
        core: UnixListener,
        backend: UnixListener,
    }

    impl SocketFixture {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "pronk-pipewire-remote-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&directory).unwrap();
            let paths = ClassifiedSocketPaths::in_runtime_dir(&directory).unwrap();
            let core = UnixListener::bind(paths.core()).unwrap();
            let backend = UnixListener::bind(paths.backend()).unwrap();
            Self {
                directory,
                paths,
                core,
                backend,
            }
        }
    }

    impl Drop for SocketFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.paths.core());
            let _ = std::fs::remove_file(self.paths.backend());
            let _ = std::fs::remove_dir(&self.directory);
        }
    }

    #[test]
    fn classified_paths_require_distinct_absolute_unix_addresses() {
        assert!(matches!(
            ClassifiedSocketPaths::new("relative-core", "/tmp/backend"),
            Err(ClassifiedSocketPathsError::NotAbsolute {
                role: PipeWireConnectionRole::CoreProducer,
                ..
            })
        ));
        assert!(matches!(
            ClassifiedSocketPaths::new("/tmp/same", "/tmp/same"),
            Err(ClassifiedSocketPathsError::SamePath)
        ));

        let overlong = PathBuf::from(format!("/tmp/{}", "x".repeat(200)));
        assert!(matches!(
            ClassifiedSocketPaths::new(overlong, "/tmp/backend"),
            Err(ClassifiedSocketPathsError::InvalidUnixAddress { .. })
        ));

        let with_nul = PathBuf::from(OsString::from_vec(b"/tmp/core\0bad".to_vec()));
        assert!(matches!(
            ClassifiedSocketPaths::new(with_nul, "/tmp/backend"),
            Err(ClassifiedSocketPathsError::InvalidUnixAddress { .. })
        ));
    }

    #[tokio::test]
    async fn mints_fresh_distinct_untouched_classified_connections() {
        let fixture = SocketFixture::new();
        let provider = ClassifiedSocketRemoteProvider::new(fixture.paths.clone());

        let producer = provider.create_producer_remote().await.unwrap();
        let first = provider
            .create_backend_remotes(
                "session-a",
                "chromiacast",
                NonZeroU64::new(7).unwrap(),
                true,
            )
            .await
            .unwrap();
        let replacement = provider
            .create_backend_remotes(
                "session-a",
                "chromiacast",
                NonZeroU64::new(8).unwrap(),
                false,
            )
            .await
            .unwrap();

        let raw_fds = HashSet::from([
            producer.as_fd().as_raw_fd(),
            first.video().as_fd().as_raw_fd(),
            first.audio().unwrap().as_fd().as_raw_fd(),
            replacement.video().as_fd().as_raw_fd(),
        ]);
        assert_eq!(raw_fds.len(), 4);
        for raw_fd in raw_fds {
            let flags = FdFlag::from_bits_truncate(fcntl(raw_fd, FcntlArg::F_GETFD).unwrap());
            assert!(flags.contains(FdFlag::FD_CLOEXEC));
        }

        let (core_peer, _) = fixture.core.accept().await.unwrap();
        assert_untouched(&core_peer);
        for _ in 0..3 {
            let (backend_peer, _) = fixture.backend.accept().await.unwrap();
            assert_untouched(&backend_peer);
        }

        assert!(matches!(
            producer.into_remote(),
            PipeWireRemote::Connected(_)
        ));
        let (_video, audio) = first.into_parts();
        assert!(audio.is_some());
        let (_video, audio) = replacement.into_parts();
        assert!(audio.is_none());
    }

    fn assert_untouched(stream: &UnixStream) {
        let mut byte = [0_u8; 1];
        let error = stream.try_read(&mut byte).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::WouldBlock);
        let credentials = getsockopt(stream, sockopt::PeerCredentials).unwrap();
        assert_eq!(credentials.uid(), Uid::effective().as_raw());
    }

    #[tokio::test]
    async fn rejects_a_pipewire_peer_with_the_wrong_uid() {
        let fixture = SocketFixture::new();
        let wrong_uid = Uid::from_raw(Uid::effective().as_raw().wrapping_add(1));
        let provider = ClassifiedSocketRemoteProvider::with_expected_server_uid(
            fixture.paths.clone(),
            wrong_uid,
        );

        assert!(matches!(
            provider.create_producer_remote().await,
            Err(RemoteProviderError::WrongPeerUid { .. })
        ));
        let (mut peer, _) = fixture.core.accept().await.unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    }

    struct FailSecondConnector {
        calls: AtomicUsize,
        first_peer: Mutex<Option<StdUnixStream>>,
    }

    #[async_trait]
    impl SocketConnector for FailSecondConnector {
        async fn connect(
            &self,
            role: PipeWireConnectionRole,
            path: &Path,
            _expected_server_uid: Uid,
        ) -> Result<OwnedFd, RemoteProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let (client, peer) = StdUnixStream::pair().unwrap();
                *self.first_peer.lock().unwrap() = Some(peer);
                Ok(OwnedFd::from(client))
            } else {
                Err(RemoteProviderError::Connect {
                    role,
                    path: path.to_owned(),
                    source: std::io::Error::new(ErrorKind::ConnectionRefused, "injected"),
                })
            }
        }
    }

    #[tokio::test]
    async fn closes_video_if_the_audio_connection_fails() {
        let fixture = SocketFixture::new();
        let provider = ClassifiedSocketRemoteProvider::new(fixture.paths.clone());
        let connector = FailSecondConnector {
            calls: AtomicUsize::new(0),
            first_peer: Mutex::new(None),
        };

        assert!(matches!(
            provider
                .create_backend_remotes_with(
                    &connector,
                    "session-a",
                    "chromiacast",
                    NonZeroU64::new(1).unwrap(),
                    true,
                )
                .await,
            Err(RemoteProviderError::Connect { .. })
        ));
        assert_eq!(connector.calls.load(Ordering::SeqCst), 2);

        let mut peer = connector.first_peer.lock().unwrap().take().unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[tokio::test]
    async fn rejects_invalid_context_before_connecting() {
        let fixture = SocketFixture::new();
        let provider = ClassifiedSocketRemoteProvider::new(fixture.paths.clone());
        assert!(matches!(
            provider
                .create_backend_remotes("", "chromiacast", NonZeroU64::new(1).unwrap(), false)
                .await,
            Err(RemoteProviderError::InvalidContext {
                field: "session ID"
            })
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), fixture.backend.accept())
                .await
                .is_err()
        );
    }
}
