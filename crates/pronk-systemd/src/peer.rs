use nix::unistd::Uid;
use thiserror::Error;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

const SYSTEMD_INVOCATION_ID_BYTES: usize = 16;

/// Root-owned startup policy for authenticating a backend's Pronk peer.
///
/// Packaged services set `PRONK_BACKEND_EXPECTED_PEER_UNIT` and select the
/// systemd manager scope with `PRONK_BACKEND_EXPECTED_PEER_BUS`. The unmanaged
/// variant exists only for process-boundary tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendPeerPolicy {
    SystemdUnit {
        expected_unit: String,
        manager_bus: ManagerBus,
    },
    UnmanagedTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerBus {
    Session,
    System,
}

impl BackendPeerPolicy {
    pub fn from_environment() -> Result<Self, BackendPeerPolicyError> {
        let unmanaged = std::env::var("PRONK_BACKEND_ALLOW_UNMANAGED_PEER").ok();
        let expected_unit = std::env::var("PRONK_BACKEND_EXPECTED_PEER_UNIT").ok();
        let manager_bus = std::env::var("PRONK_BACKEND_EXPECTED_PEER_BUS").ok();
        match (unmanaged.as_deref(), expected_unit, manager_bus.as_deref()) {
            (Some("1"), None, None) => Ok(Self::UnmanagedTest),
            (None, Some(expected_unit), manager_bus) => Ok(Self::SystemdUnit {
                expected_unit: validate_service_unit_name(&expected_unit)?.to_owned(),
                manager_bus: parse_manager_bus(manager_bus)?,
            }),
            (Some(value), _, _) => Err(BackendPeerPolicyError::InvalidUnmanagedOverride(
                value.into(),
            )),
            (None, None, _) => Err(BackendPeerPolicyError::MissingExpectedUnit),
        }
    }

    pub fn is_unmanaged_test(&self) -> bool {
        matches!(self, Self::UnmanagedTest)
    }

    pub async fn validate(&self, connection: &Connection) -> Result<(), BackendPeerPolicyError> {
        match self {
            Self::SystemdUnit {
                expected_unit,
                manager_bus,
            } => {
                let validator = match manager_bus {
                    ManagerBus::Session => SystemdPeerValidator::session(expected_unit).await?,
                    ManagerBus::System => SystemdPeerValidator::system(expected_unit).await?,
                };
                validator
                    .validate_peer(connection)
                    .await
                    .map(|_| ())
                    .map_err(Into::into)
            }
            Self::UnmanagedTest => Ok(()),
        }
    }
}

#[derive(Debug, Error)]
pub enum BackendPeerPolicyError {
    #[error("missing root-owned PRONK_BACKEND_EXPECTED_PEER_UNIT peer policy")]
    MissingExpectedUnit,
    #[error("invalid PRONK_BACKEND_ALLOW_UNMANAGED_PEER value {0:?} or ambiguous peer policy")]
    InvalidUnmanagedOverride(String),
    #[error("invalid PRONK_BACKEND_EXPECTED_PEER_BUS value {0:?}")]
    InvalidManagerBus(String),
    #[error(transparent)]
    Systemd(#[from] PeerServiceValidationError),
}

#[derive(Debug, Clone)]
pub struct SystemdPeerValidator {
    manager_connection: Connection,
    expected_unit: String,
}

impl SystemdPeerValidator {
    pub fn new(
        manager_connection: Connection,
        expected_unit: impl Into<String>,
    ) -> Result<Self, PeerServiceValidationError> {
        let expected_unit = expected_unit.into();
        validate_service_unit_name(&expected_unit)?;
        Ok(Self {
            manager_connection,
            expected_unit,
        })
    }

    pub async fn session(
        expected_unit: impl Into<String>,
    ) -> Result<Self, PeerServiceValidationError> {
        Self::new(
            Connection::session()
                .await
                .map_err(PeerServiceValidationError::ManagerConnection)?,
            expected_unit,
        )
    }

    pub async fn system(
        expected_unit: impl Into<String>,
    ) -> Result<Self, PeerServiceValidationError> {
        Self::new(
            Connection::system()
                .await
                .map_err(PeerServiceValidationError::ManagerConnection)?,
            expected_unit,
        )
    }

    pub async fn validate_peer(
        &self,
        peer_connection: &Connection,
    ) -> Result<PeerServiceIdentity, PeerServiceValidationError> {
        let credentials = peer_connection
            .peer_credentials()
            .await
            .map_err(PeerServiceValidationError::PeerCredentials)?;
        let peer_uid = credentials
            .unix_user_id()
            .ok_or(PeerServiceValidationError::MissingPeerUid)?;
        let expected_uid = Uid::effective().as_raw();
        if peer_uid != expected_uid {
            return Err(PeerServiceValidationError::WrongPeerUid {
                expected: expected_uid,
                actual: peer_uid,
            });
        }
        let peer_pid = credentials
            .process_id()
            .filter(|pid| *pid != 0)
            .ok_or(PeerServiceValidationError::MissingPeerPid)?;
        self.validate_pid(peer_pid).await
    }

    async fn validate_pid(
        &self,
        peer_pid: u32,
    ) -> Result<PeerServiceIdentity, PeerServiceValidationError> {
        let manager = SystemdManagerProxy::new(&self.manager_connection)
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        let path = manager
            .get_unit_by_pid(peer_pid)
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        let unit = SystemdUnitProxy::builder(&self.manager_connection)
            .path(path.clone())
            .map_err(PeerServiceValidationError::ManagerProtocol)?
            .build()
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        let unit_id = unit
            .id()
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        if unit_id != self.expected_unit {
            return Err(PeerServiceValidationError::WrongUnit {
                expected: self.expected_unit.clone(),
                actual: unit_id,
            });
        }
        let active_state = unit
            .active_state()
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        if !matches!(active_state.as_str(), "activating" | "active") {
            return Err(PeerServiceValidationError::InactiveUnit {
                unit: unit_id,
                state: active_state,
            });
        }
        let invocation_id = unit
            .invocation_id()
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        let invocation_id: [u8; SYSTEMD_INVOCATION_ID_BYTES] = invocation_id
            .try_into()
            .map_err(|_| PeerServiceValidationError::InvalidInvocationId)?;
        if invocation_id.iter().all(|byte| *byte == 0) {
            return Err(PeerServiceValidationError::InvalidInvocationId);
        }

        let service = SystemdServiceProxy::builder(&self.manager_connection)
            .path(path)
            .map_err(PeerServiceValidationError::ManagerProtocol)?
            .build()
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        let main_pid = service
            .main_pid()
            .await
            .map_err(PeerServiceValidationError::ManagerProtocol)?;
        if main_pid != peer_pid {
            return Err(PeerServiceValidationError::WrongMainPid {
                unit: unit_id,
                expected: main_pid,
                actual: peer_pid,
            });
        }

        Ok(PeerServiceIdentity {
            peer_pid,
            unit_id: self.expected_unit.clone(),
            invocation_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerServiceIdentity {
    pub peer_pid: u32,
    pub unit_id: String,
    pub invocation_id: [u8; SYSTEMD_INVOCATION_ID_BYTES],
}

fn validate_service_unit_name(unit: &str) -> Result<&str, PeerServiceValidationError> {
    let Some(stem) = unit.strip_suffix(".service") else {
        return Err(PeerServiceValidationError::InvalidExpectedUnit(unit.into()));
    };
    if stem.is_empty()
        || stem.len() > 128
        || !stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(PeerServiceValidationError::InvalidExpectedUnit(unit.into()));
    }
    Ok(unit)
}

fn parse_manager_bus(value: Option<&str>) -> Result<ManagerBus, BackendPeerPolicyError> {
    match value.unwrap_or("session") {
        "session" => Ok(ManagerBus::Session),
        "system" => Ok(ManagerBus::System),
        value => Err(BackendPeerPolicyError::InvalidManagerBus(value.into())),
    }
}

#[derive(Debug, Error)]
pub enum PeerServiceValidationError {
    #[error("invalid expected systemd service unit {0:?}")]
    InvalidExpectedUnit(String),
    #[error("cannot connect to the selected systemd manager: {0}")]
    ManagerConnection(zbus::Error),
    #[error("cannot read P2P peer credentials: {0}")]
    PeerCredentials(std::io::Error),
    #[error("P2P peer credentials have no Unix UID")]
    MissingPeerUid,
    #[error("P2P peer UID {actual} differs from effective UID {expected}")]
    WrongPeerUid { expected: u32, actual: u32 },
    #[error("P2P peer credentials have no nonzero PID")]
    MissingPeerPid,
    #[error("systemd peer validation failed: {0}")]
    ManagerProtocol(zbus::Error),
    #[error("P2P peer belongs to unit {actual:?}, expected {expected:?}")]
    WrongUnit { expected: String, actual: String },
    #[error("P2P peer unit {unit} has state {state:?}")]
    InactiveUnit { unit: String, state: String },
    #[error("P2P peer unit has an invalid invocation ID")]
    InvalidInvocationId,
    #[error("P2P peer PID {actual} is not unit {unit}'s main PID {expected}")]
    WrongMainPid {
        unit: String,
        expected: u32,
        actual: u32,
    },
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    gen_blocking = false
)]
trait SystemdManager {
    #[zbus(name = "GetUnitByPID")]
    fn get_unit_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1",
    gen_blocking = false
)]
trait SystemdUnit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "InvocationID")]
    fn invocation_id(&self) -> zbus::Result<Vec<u8>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Service",
    default_service = "org.freedesktop.systemd1",
    gen_blocking = false
)]
trait SystemdService {
    #[zbus(property, name = "MainPID")]
    fn main_pid(&self) -> zbus::Result<u32>;
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixStream;
    use zbus::connection::{AuthMechanism, Builder};
    use zbus::Guid;

    use super::*;

    const FAKE_UNIT_PATH: &str = "/org/freedesktop/systemd1/unit/pronk_2eservice";
    const INVOCATION_ID: [u8; SYSTEMD_INVOCATION_ID_BYTES] = [0x5a; 16];

    #[derive(Debug)]
    struct FakeManager;

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeManager {
        #[zbus(name = "GetUnitByPID")]
        fn get_unit_by_pid(&self, pid: u32) -> zbus::fdo::Result<OwnedObjectPath> {
            if pid != std::process::id() {
                return Err(zbus::fdo::Error::Failed("unexpected PID".into()));
            }
            OwnedObjectPath::try_from(FAKE_UNIT_PATH)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }
    }

    #[derive(Debug)]
    struct FakeUnit;

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl FakeUnit {
        #[zbus(property)]
        fn id(&self) -> &str {
            "pronk.service"
        }

        #[zbus(property)]
        fn active_state(&self) -> &str {
            "active"
        }

        #[zbus(property, name = "InvocationID")]
        fn invocation_id(&self) -> Vec<u8> {
            INVOCATION_ID.into()
        }
    }

    #[derive(Debug)]
    struct FakeService;

    #[zbus::interface(name = "org.freedesktop.systemd1.Service")]
    impl FakeService {
        #[zbus(property, name = "MainPID")]
        fn main_pid(&self) -> u32 {
            std::process::id()
        }
    }

    #[test]
    fn expected_service_name_is_fixed_and_bounded() {
        assert!(validate_service_unit_name("pronk.service").is_ok());
        assert!(validate_service_unit_name("pronk@.service").is_err());
        assert!(validate_service_unit_name("../pronk.service").is_err());
        assert!(validate_service_unit_name("pronk.socket").is_err());
    }

    #[tokio::test]
    async fn validates_exact_systemd_unit_main_pid_and_invocation() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = Builder::unix_stream(server_stream)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at("/org/freedesktop/systemd1", FakeManager)
            .unwrap()
            .serve_at(FAKE_UNIT_PATH, FakeUnit)
            .unwrap()
            .serve_at(FAKE_UNIT_PATH, FakeService)
            .unwrap();
        let client = Builder::unix_stream(client_stream)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        let (server_connection, client_connection) =
            tokio::try_join!(server.build(), client.build()).unwrap();

        let identity = SystemdPeerValidator::new(client_connection, "pronk.service")
            .unwrap()
            .validate_pid(std::process::id())
            .await
            .unwrap();
        assert_eq!(identity.peer_pid, std::process::id());
        assert_eq!(identity.unit_id, "pronk.service");
        assert_eq!(identity.invocation_id, INVOCATION_ID);
        drop(server_connection);
    }
}
