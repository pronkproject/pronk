use async_trait::async_trait;
use pronk_backend_protocol::BackendInfo;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::{
    BackendEndpoint, BackendInstanceControlError, BackendRegistrationValidator,
    RegistrationValidationError,
};

#[derive(Debug, Clone)]
pub struct SystemdRegistrationValidator {
    connection: Connection,
}

impl SystemdRegistrationValidator {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    pub async fn session() -> Result<Self, zbus::Error> {
        Ok(Self::new(Connection::session().await?))
    }
}

#[async_trait]
impl BackendRegistrationValidator for SystemdRegistrationValidator {
    async fn validate(
        &self,
        endpoint: &BackendEndpoint,
        info: &BackendInfo,
        backend_pid: u32,
    ) -> Result<(), RegistrationValidationError> {
        let invocation_id = decode_invocation_id(&info.invocation_id)?;
        let manager = SystemdManagerProxy::new(&self.connection)
            .await
            .map_err(service_error)?;
        let path = manager
            .get_unit_by_invocation_id(invocation_id)
            .await
            .map_err(service_error)?;
        validate_unit(
            &self.connection,
            &path,
            endpoint,
            &info.activation_instance,
            &info.invocation_id,
            Some(backend_pid),
        )
        .await
        .map(|_| ())
    }

    async fn stop_validated_instance(
        &self,
        endpoint: &BackendEndpoint,
        info: &BackendInfo,
    ) -> Result<(), BackendInstanceControlError> {
        let invocation_id = decode_invocation_id(&info.invocation_id).map_err(control_error)?;
        let manager = SystemdManagerProxy::new(&self.connection)
            .await
            .map_err(control_service_error)?;
        let path = manager
            .get_unit_by_invocation_id(invocation_id)
            .await
            .map_err(control_service_error)?;
        let unit_id = validate_unit(
            &self.connection,
            &path,
            endpoint,
            &info.activation_instance,
            &info.invocation_id,
            None,
        )
        .await
        .map_err(control_error)?;
        manager
            .stop_unit(&unit_id, "replace")
            .await
            .map_err(control_service_error)?;
        Ok(())
    }
}

async fn validate_unit(
    connection: &Connection,
    path: &OwnedObjectPath,
    endpoint: &BackendEndpoint,
    activation_instance: &str,
    invocation_id_hex: &str,
    expected_main_pid: Option<u32>,
) -> Result<String, RegistrationValidationError> {
    let unit = SystemdUnitProxy::builder(connection)
        .path(path.clone())
        .map_err(service_error)?
        .build()
        .await
        .map_err(service_error)?;
    let id = unit.id().await.map_err(service_error)?;
    let actual_instance = template_instance(endpoint.service_template(), &id).ok_or_else(|| {
        RegistrationValidationError::Service(format!(
            "invocation {invocation_id_hex} belongs to unexpected unit {id:?}"
        ))
    })?;
    if actual_instance != activation_instance {
        return Err(RegistrationValidationError::WrongActivationInstance {
            expected: actual_instance.into(),
            actual: activation_instance.into(),
        });
    }

    let active_state = unit.active_state().await.map_err(service_error)?;
    if !matches!(active_state.as_str(), "activating" | "active") {
        return Err(RegistrationValidationError::Service(format!(
            "backend unit {id} has state {active_state:?}"
        )));
    }
    let unit_invocation_id = unit.invocation_id().await.map_err(service_error)?;
    let expected_invocation_id = decode_invocation_id(invocation_id_hex)?;
    if unit_invocation_id != expected_invocation_id {
        return Err(RegistrationValidationError::Service(format!(
            "backend unit {id} changed invocation ID during registration"
        )));
    }
    let triggered_by = unit.triggered_by().await.map_err(service_error)?;
    let expected_socket = endpoint.socket_unit();
    if !triggered_by.iter().any(|unit| unit == &expected_socket) {
        return Err(RegistrationValidationError::Service(format!(
            "backend unit {id} was not triggered by {expected_socket}"
        )));
    }

    let service = SystemdServiceProxy::builder(connection)
        .path(path.clone())
        .map_err(service_error)?
        .build()
        .await
        .map_err(service_error)?;
    let main_pid = service.main_pid().await.map_err(service_error)?;
    if main_pid == 0 {
        return Err(RegistrationValidationError::Service(format!(
            "backend unit {id} has no main PID"
        )));
    }
    if let Some(expected_main_pid) = expected_main_pid {
        if main_pid != expected_main_pid {
            return Err(RegistrationValidationError::Service(format!(
                "backend unit {id} has main PID {main_pid}, but D-Bus messages came from PID {expected_main_pid}"
            )));
        }
    }
    Ok(id)
}

fn decode_invocation_id(value: &str) -> Result<Vec<u8>, RegistrationValidationError> {
    if value.len() != 32 {
        return Err(RegistrationValidationError::Service(
            "systemd invocation ID must contain 32 lowercase hexadecimal characters".into(),
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = decode_hex_digit(pair[0])?;
            let low = decode_hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn decode_hex_digit(value: u8) -> Result<u8, RegistrationValidationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(RegistrationValidationError::Service(
            "systemd invocation ID must contain 32 lowercase hexadecimal characters".into(),
        )),
    }
}

fn template_instance<'a>(template: &str, unit_id: &'a str) -> Option<&'a str> {
    let prefix = template.strip_suffix("@.service")?;
    let instance = unit_id.strip_prefix(&format!("{prefix}@"))?;
    let instance = instance.strip_suffix(".service")?;
    (!instance.is_empty()).then_some(instance)
}

fn service_error(error: zbus::Error) -> RegistrationValidationError {
    RegistrationValidationError::Service(error.to_string())
}

fn control_error(error: RegistrationValidationError) -> BackendInstanceControlError {
    BackendInstanceControlError::Service(error.to_string())
}

fn control_service_error(error: zbus::Error) -> BackendInstanceControlError {
    BackendInstanceControlError::Service(error.to_string())
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    gen_blocking = false
)]
trait SystemdManager {
    #[zbus(name = "GetUnitByInvocationID")]
    fn get_unit_by_invocation_id(&self, invocation_id: Vec<u8>) -> zbus::Result<OwnedObjectPath>;

    #[zbus(name = "StopUnit")]
    fn stop_unit(&self, unit: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
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

    #[zbus(property)]
    fn triggered_by(&self) -> zbus::Result<Vec<String>>;
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
    use std::sync::{Arc, Mutex};

    use pronk_backend_protocol::{backend_host_builder, backend_peer_builder};
    use tokio::net::UnixStream;

    use super::*;

    const FAKE_UNIT_PATH: &str = "/org/freedesktop/systemd1/unit/pronk_2dbackend_2dmock";
    const FAKE_JOB_PATH: &str = "/org/freedesktop/systemd1/job/42";

    #[derive(Debug)]
    struct FakeManager {
        invocation_id: Vec<u8>,
        stopped_units: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeManager {
        #[zbus(name = "GetUnitByInvocationID")]
        fn get_unit_by_invocation_id(
            &self,
            invocation_id: Vec<u8>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            if invocation_id != self.invocation_id {
                return Err(zbus::fdo::Error::Failed("unknown invocation ID".into()));
            }
            OwnedObjectPath::try_from(FAKE_UNIT_PATH)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }

        #[zbus(name = "StopUnit")]
        fn stop_unit(&self, unit: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
            if mode != "replace" {
                return Err(zbus::fdo::Error::InvalidArgs("unexpected job mode".into()));
            }
            self.stopped_units
                .lock()
                .expect("fake manager mutex poisoned")
                .push(unit.into());
            OwnedObjectPath::try_from(FAKE_JOB_PATH)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }
    }

    #[derive(Debug, Clone)]
    struct FakeUnit {
        invocation_id: Vec<u8>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl FakeUnit {
        #[zbus(property)]
        fn id(&self) -> &str {
            "pronk-backend-mock@connection-7.service"
        }

        #[zbus(property)]
        fn active_state(&self) -> &str {
            "activating"
        }

        #[zbus(property, name = "InvocationID")]
        fn invocation_id(&self) -> Vec<u8> {
            self.invocation_id.clone()
        }

        #[zbus(property)]
        fn triggered_by(&self) -> Vec<String> {
            vec!["pronk-backend-mock.socket".into()]
        }
    }

    #[derive(Debug)]
    struct FakeService;

    #[zbus::interface(name = "org.freedesktop.systemd1.Service")]
    impl FakeService {
        #[zbus(property, name = "MainPID")]
        fn main_pid(&self) -> u32 {
            4242
        }
    }

    #[test]
    fn decodes_only_canonical_systemd_invocation_ids() {
        assert_eq!(
            decode_invocation_id("00112233445566778899aabbccddeeff").unwrap(),
            vec![
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
        assert!(decode_invocation_id("00112233445566778899AABBCCDDEEFF").is_err());
        assert!(decode_invocation_id("short").is_err());
    }

    #[test]
    fn extracts_only_instances_of_the_fixed_template() {
        assert_eq!(
            template_instance(
                "pronk-backend-mock@.service",
                "pronk-backend-mock@connection-7.service"
            ),
            Some("connection-7")
        );
        assert_eq!(
            template_instance(
                "pronk-backend-mock@.service",
                "pronk-backend-other@connection-7.service"
            ),
            None
        );
        assert_eq!(
            template_instance("pronk-backend-mock@.service", "pronk-backend-mock@.service"),
            None
        );
    }

    #[test]
    fn endpoint_derives_the_accept_socket_unit() {
        let endpoint = BackendEndpoint::new(
            "mock",
            "/run/user/1000/pronk/backends/mock.sock",
            "pronk-backend-mock@.service",
        )
        .unwrap();
        assert_eq!(endpoint.socket_unit(), "pronk-backend-mock.socket");
    }

    #[tokio::test]
    async fn validates_an_invocation_against_the_systemd_interfaces() {
        let invocation_hex = "00112233445566778899aabbccddeeff";
        let invocation_id = decode_invocation_id(invocation_hex).unwrap();
        let stopped_units = Arc::new(Mutex::new(Vec::new()));
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = backend_host_builder(server_stream)
            .unwrap()
            .serve_at(
                "/org/freedesktop/systemd1",
                FakeManager {
                    invocation_id: invocation_id.clone(),
                    stopped_units: stopped_units.clone(),
                },
            )
            .unwrap()
            .serve_at(
                FAKE_UNIT_PATH,
                FakeUnit {
                    invocation_id: invocation_id.clone(),
                },
            )
            .unwrap()
            .serve_at(FAKE_UNIT_PATH, FakeService)
            .unwrap();
        let client = backend_peer_builder(client_stream);
        let (server_connection, client_connection) =
            tokio::try_join!(server.build(), client.build()).unwrap();

        let endpoint = BackendEndpoint::new(
            "mock",
            "/run/user/1000/pronk/backends/mock.sock",
            "pronk-backend-mock@.service",
        )
        .unwrap();
        let info = BackendInfo::v1("mock", "Mock", "0", "connection-7", invocation_hex);
        SystemdRegistrationValidator::new(client_connection.clone())
            .validate(&endpoint, &info, 4242)
            .await
            .unwrap();
        SystemdRegistrationValidator::new(client_connection.clone())
            .stop_validated_instance(&endpoint, &info)
            .await
            .unwrap();
        assert_eq!(
            *stopped_units.lock().unwrap(),
            vec!["pronk-backend-mock@connection-7.service"]
        );

        let wrong_pid = SystemdRegistrationValidator::new(client_connection.clone())
            .validate(&endpoint, &info, 4243)
            .await
            .unwrap_err();
        assert!(matches!(wrong_pid, RegistrationValidationError::Service(_)));
        assert!(wrong_pid
            .to_string()
            .contains("D-Bus messages came from PID 4243"));

        let mut wrong_instance = info;
        wrong_instance.activation_instance = "connection-8".into();
        assert!(matches!(
            SystemdRegistrationValidator::new(client_connection)
                .validate(&endpoint, &wrong_instance, 4242)
                .await,
            Err(RegistrationValidationError::WrongActivationInstance { .. })
        ));
        drop(server_connection);
    }
}
