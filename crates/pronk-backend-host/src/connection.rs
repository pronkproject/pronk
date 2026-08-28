use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use pronk_backend_protocol::{
    backend_host_builder, require_same_uid, Backend1Proxy, BackendInfo, RegistrationReply,
    Validate, BACKEND_HOST_PATH, PROTOCOL_MINOR,
};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::time::timeout;
use zbus::object_server::ResponseDispatchNotifier;
use zbus::{Connection, MessageStream};

use crate::message_credentials::MessageCredentialsUnixStream;
use crate::BackendEndpoint;

pub const BACKEND_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const BACKEND_METHOD_TIMEOUT: Duration = Duration::from_secs(5);

#[async_trait]
pub trait BackendRegistrationValidator: Send + Sync {
    async fn validate(
        &self,
        endpoint: &BackendEndpoint,
        info: &BackendInfo,
        backend_pid: u32,
    ) -> Result<(), RegistrationValidationError>;

    async fn stop_validated_instance(
        &self,
        _endpoint: &BackendEndpoint,
        _info: &BackendInfo,
    ) -> Result<(), BackendInstanceControlError> {
        Err(BackendInstanceControlError::Unsupported)
    }
}

#[derive(Debug, Clone)]
pub struct ExactRegistrationValidator {
    activation_instance: String,
    invocation_id: String,
}

impl ExactRegistrationValidator {
    pub fn new(activation_instance: impl Into<String>, invocation_id: impl Into<String>) -> Self {
        Self {
            activation_instance: activation_instance.into(),
            invocation_id: invocation_id.into(),
        }
    }
}

#[async_trait]
impl BackendRegistrationValidator for ExactRegistrationValidator {
    async fn validate(
        &self,
        _endpoint: &BackendEndpoint,
        info: &BackendInfo,
        _backend_pid: u32,
    ) -> Result<(), RegistrationValidationError> {
        if info.activation_instance != self.activation_instance {
            return Err(RegistrationValidationError::WrongActivationInstance {
                expected: self.activation_instance.clone(),
                actual: info.activation_instance.clone(),
            });
        }
        if info.invocation_id != self.invocation_id {
            return Err(RegistrationValidationError::WrongInvocationId {
                expected: self.invocation_id.clone(),
                actual: info.invocation_id.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct BackendConnection {
    endpoint: BackendEndpoint,
    connection: Connection,
    info: BackendInfo,
    connection_generation: u64,
    negotiated_minor: u16,
}

impl BackendConnection {
    pub async fn connect(
        endpoint: BackendEndpoint,
        connection_generation: u64,
        registration_validator: Arc<dyn BackendRegistrationValidator>,
    ) -> Result<Self, BackendConnectError> {
        if connection_generation == 0 {
            return Err(BackendConnectError::ZeroConnectionGeneration);
        }
        let stream = timeout(
            BACKEND_HANDSHAKE_TIMEOUT,
            MessageCredentialsUnixStream::connect(endpoint.socket_path()),
        )
        .await
        .map_err(|_| BackendConnectError::ConnectTimeout)?
        .map_err(BackendConnectError::Connect)?;
        let (registration_tx, registration_rx) = oneshot::channel();
        let host = RegistrationHost::new(
            endpoint.clone(),
            connection_generation,
            registration_validator,
            registration_tx,
        );
        let builder = backend_host_builder(stream)
            .map_err(BackendConnectError::Build)?
            .serve_at(BACKEND_HOST_PATH, host)
            .map_err(BackendConnectError::Build)?;
        let connection = timeout(BACKEND_HANDSHAKE_TIMEOUT, builder.build())
            .await
            .map_err(|_| BackendConnectError::HandshakeTimeout)?
            .map_err(BackendConnectError::Build)?;
        require_same_uid(&connection)
            .await
            .map_err(|error| BackendConnectError::PeerIdentity(error.to_string()))?;

        let mut registration_rx = registration_rx;
        let mut registration_messages = MessageStream::from(&connection);
        let registration = timeout(BACKEND_HANDSHAKE_TIMEOUT, async {
            loop {
                tokio::select! {
                    registration = &mut registration_rx => {
                        return registration
                            .map_err(|_| BackendConnectError::RegistrationChannelClosed);
                    }
                    message = registration_messages.next() => match message {
                        None | Some(Err(zbus::Error::InputOutput(_))) => {
                            return Err(BackendConnectError::RegistrationConnectionClosed);
                        }
                        Some(Err(error)) => {
                            return Err(BackendConnectError::RegistrationProtocol(error));
                        }
                        Some(Ok(_)) => {}
                    }
                }
            }
        })
        .await
        .map_err(|_| BackendConnectError::RegistrationTimeout)??;
        if let Err(error) = registration.validation {
            return Err(BackendConnectError::RegistrationRejected(error));
        }

        let proxy = Backend1Proxy::new(&connection)
            .await
            .map_err(BackendConnectError::Proxy)?;
        let returned_info = timeout(BACKEND_METHOD_TIMEOUT, proxy.get_info())
            .await
            .map_err(|_| BackendConnectError::GetInfoTimeout)?
            .map_err(BackendConnectError::GetInfo)?;
        if returned_info != registration.info {
            return Err(BackendConnectError::GetInfoMismatch);
        }

        Ok(Self {
            endpoint,
            connection,
            negotiated_minor: negotiate_protocol_minor(
                PROTOCOL_MINOR,
                registration.info.protocol_minor,
            ),
            info: registration.info,
            connection_generation,
        })
    }

    pub fn endpoint(&self) -> &BackendEndpoint {
        &self.endpoint
    }

    pub fn info(&self) -> &BackendInfo {
        &self.info
    }

    pub fn connection_generation(&self) -> u64 {
        self.connection_generation
    }

    pub fn negotiated_minor(&self) -> u16 {
        self.negotiated_minor
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub async fn shutdown(&self) -> Result<(), BackendConnectionError> {
        let proxy = Backend1Proxy::new(&self.connection)
            .await
            .map_err(BackendConnectionError::Protocol)?;
        timeout(BACKEND_METHOD_TIMEOUT, proxy.shutdown())
            .await
            .map_err(|_| BackendConnectionError::MethodTimeout("Shutdown"))?
            .map_err(BackendConnectionError::Protocol)
    }

    pub async fn wait_for_eof(&self) -> Result<(), BackendConnectionError> {
        let mut messages = MessageStream::from(&self.connection);
        while let Some(message) = messages.next().await {
            match message {
                Ok(_) => {}
                // Depending on how the connected stream was accepted, peer
                // shutdown is surfaced either by ending MessageStream or by
                // its final read returning an I/O error. Both mean the P2P
                // transport is gone; decoding and protocol errors remain
                // failures.
                Err(zbus::Error::InputOutput(_)) => return Ok(()),
                Err(error) => return Err(BackendConnectionError::Protocol(error)),
            }
        }
        Ok(())
    }

    pub async fn close(self) -> Result<(), BackendConnectionError> {
        self.connection
            .close()
            .await
            .map_err(BackendConnectionError::Protocol)
    }
}

#[derive(Debug)]
struct RegistrationOutcome {
    info: BackendInfo,
    validation: Result<(), String>,
}

#[derive(Clone)]
struct RegistrationHost {
    shared: Arc<RegistrationHostShared>,
}

struct RegistrationHostShared {
    endpoint: BackendEndpoint,
    connection_generation: u64,
    registration_validator: Arc<dyn BackendRegistrationValidator>,
    registration_tx: Mutex<Option<oneshot::Sender<RegistrationOutcome>>>,
    call_count: AtomicUsize,
}

impl RegistrationHost {
    fn new(
        endpoint: BackendEndpoint,
        connection_generation: u64,
        registration_validator: Arc<dyn BackendRegistrationValidator>,
        registration_tx: oneshot::Sender<RegistrationOutcome>,
    ) -> Self {
        Self {
            shared: Arc::new(RegistrationHostShared {
                endpoint,
                connection_generation,
                registration_validator,
                registration_tx: Mutex::new(Some(registration_tx)),
                call_count: AtomicUsize::new(0),
            }),
        }
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk.BackendHost1")]
impl RegistrationHost {
    async fn register_backend(
        &self,
        #[zbus(connection)] connection: &Connection,
        info: BackendInfo,
    ) -> zbus::fdo::Result<ResponseDispatchNotifier<RegistrationReply>> {
        let call_count = self.shared.call_count.fetch_add(1, Ordering::SeqCst) + 1;
        if call_count != 1 {
            return Err(zbus::fdo::Error::Failed(
                "RegisterBackend is callable exactly once".into(),
            ));
        }

        require_same_uid(connection)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let backend_pid = connection
            .peer_credentials()
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .process_id()
            .filter(|pid| *pid != 0)
            .ok_or_else(|| {
                zbus::fdo::Error::Failed("backend peer credentials have no process ID".into())
            })?;

        let validation = validate_registration(
            &self.shared.endpoint,
            self.shared.registration_validator.as_ref(),
            &info,
            backend_pid,
        )
        .await
        .map_err(|error| error.to_string());
        let sender = self
            .shared
            .registration_tx
            .lock()
            .expect("registration mutex poisoned")
            .take();
        if let Err(error) = validation {
            if let Some(sender) = sender {
                let _ = sender.send(RegistrationOutcome {
                    info,
                    validation: Err(error.clone()),
                });
            }
            return Err(zbus::fdo::Error::InvalidArgs(error));
        }
        let reply = RegistrationReply {
            protocol_minor: negotiate_protocol_minor(PROTOCOL_MINOR, info.protocol_minor),
            connection_generation: self.shared.connection_generation,
        };
        let (reply, dispatched) = ResponseDispatchNotifier::new(reply);
        if let Some(sender) = sender {
            connection
                .executor()
                .spawn(
                    async move {
                        dispatched.await;
                        let _ = sender.send(RegistrationOutcome {
                            info,
                            validation: Ok(()),
                        });
                    },
                    "complete backend registration",
                )
                .detach();
        }
        Ok(reply)
    }
}

fn negotiate_protocol_minor(local_minor: u16, peer_minor: u16) -> u16 {
    local_minor.min(peer_minor)
}

async fn validate_registration(
    endpoint: &BackendEndpoint,
    validator: &dyn BackendRegistrationValidator,
    info: &BackendInfo,
    backend_pid: u32,
) -> Result<(), RegistrationValidationError> {
    info.validate()
        .map_err(|error| RegistrationValidationError::InvalidInfo(error.to_string()))?;
    if info.backend_id != endpoint.backend_id() {
        return Err(RegistrationValidationError::WrongBackendId {
            expected: endpoint.backend_id().into(),
            actual: info.backend_id.clone(),
        });
    }
    validator.validate(endpoint, info, backend_pid).await
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistrationValidationError {
    #[error("invalid BackendInfo: {0}")]
    InvalidInfo(String),
    #[error("backend ID {actual:?} does not match endpoint {expected:?}")]
    WrongBackendId { expected: String, actual: String },
    #[error("activation instance {actual:?} does not match {expected:?}")]
    WrongActivationInstance { expected: String, actual: String },
    #[error("invocation ID {actual:?} does not match {expected:?}")]
    WrongInvocationId { expected: String, actual: String },
    #[error("backend service validation failed: {0}")]
    Service(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BackendInstanceControlError {
    #[error("registration validator cannot stop backend instances")]
    Unsupported,
    #[error("backend service control failed: {0}")]
    Service(String),
}

#[derive(Debug, Error)]
pub enum BackendConnectError {
    #[error("connection generation must be nonzero")]
    ZeroConnectionGeneration,
    #[error("backend activation connect timed out")]
    ConnectTimeout,
    #[error("cannot connect backend activation socket: {0}")]
    Connect(std::io::Error),
    #[error("cannot build backend P2P connection: {0}")]
    Build(zbus::Error),
    #[error("backend P2P authentication timed out")]
    HandshakeTimeout,
    #[error("backend peer identity failed: {0}")]
    PeerIdentity(String),
    #[error("backend did not call RegisterBackend before the deadline")]
    RegistrationTimeout,
    #[error("backend registration channel closed")]
    RegistrationChannelClosed,
    #[error("backend P2P connection closed before registration completed")]
    RegistrationConnectionClosed,
    #[error("backend P2P protocol failed during registration: {0}")]
    RegistrationProtocol(zbus::Error),
    #[error("backend registration was rejected: {0}")]
    RegistrationRejected(String),
    #[error("cannot construct Backend1 proxy: {0}")]
    Proxy(zbus::Error),
    #[error("Backend1.GetInfo timed out")]
    GetInfoTimeout,
    #[error("Backend1.GetInfo failed: {0}")]
    GetInfo(zbus::Error),
    #[error("Backend1.GetInfo differs from the registered BackendInfo")]
    GetInfoMismatch,
}

#[derive(Debug, Error)]
pub enum BackendConnectionError {
    #[error("backend method {0} timed out")]
    MethodTimeout(&'static str),
    #[error("backend P2P protocol failed: {0}")]
    Protocol(zbus::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_registration_validator_checks_both_systemd_identities() {
        let endpoint = BackendEndpoint::new(
            "mock",
            "/run/user/1000/pronk/backends/mock.sock",
            "pronk-backend-mock@.service",
        )
        .unwrap();
        let info = BackendInfo::v1("mock", "Mock", "0", "instance", "invocation");
        let validator = ExactRegistrationValidator::new("instance", "invocation");
        validator
            .validate(&endpoint, &info, std::process::id())
            .await
            .unwrap();

        let wrong = ExactRegistrationValidator::new("other", "invocation");
        assert!(matches!(
            wrong.validate(&endpoint, &info, std::process::id()).await,
            Err(RegistrationValidationError::WrongActivationInstance { .. })
        ));
    }

    #[test]
    fn protocol_minor_negotiation_selects_the_older_peer() {
        assert_eq!(negotiate_protocol_minor(2, 4), 2);
        assert_eq!(negotiate_protocol_minor(4, 2), 2);
    }
}
