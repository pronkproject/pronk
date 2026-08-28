use std::path::{Path, PathBuf};

use pronk_backend_protocol::{BackendInfo, Validate};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendEndpoint {
    backend_id: String,
    socket_path: PathBuf,
    service_template: String,
}

impl BackendEndpoint {
    pub fn new(
        backend_id: impl Into<String>,
        socket_path: impl Into<PathBuf>,
        service_template: impl Into<String>,
    ) -> Result<Self, EndpointError> {
        let endpoint = Self {
            backend_id: backend_id.into(),
            socket_path: socket_path.into(),
            service_template: service_template.into(),
        };
        endpoint.validate()?;
        Ok(endpoint)
    }

    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn service_template(&self) -> &str {
        &self.service_template
    }

    pub fn socket_unit(&self) -> String {
        format!(
            "{}.socket",
            self.service_template
                .strip_suffix("@.service")
                .expect("validated service template")
        )
    }

    fn validate(&self) -> Result<(), EndpointError> {
        // Reuse the wire grammar without inventing a second backend-ID policy.
        let identity_probe = BackendInfo::v1(
            self.backend_id.clone(),
            "endpoint validation",
            "0",
            "validation",
            "validation",
        );
        identity_probe
            .validate()
            .map_err(|error| EndpointError::InvalidBackendId(error.to_string()))?;
        if self.socket_path.as_os_str().is_empty() {
            return Err(EndpointError::EmptySocketPath);
        }
        if !self.socket_path.is_absolute() {
            return Err(EndpointError::RelativeSocketPath {
                path: self.socket_path.clone(),
            });
        }
        validate_service_template(&self.service_template)?;
        Ok(())
    }
}

fn validate_service_template(template: &str) -> Result<(), EndpointError> {
    let Some(prefix) = template.strip_suffix("@.service") else {
        return Err(EndpointError::InvalidServiceTemplate(template.into()));
    };
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(EndpointError::InvalidServiceTemplate(template.into()));
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EndpointError {
    #[error("invalid backend ID: {0}")]
    InvalidBackendId(String),
    #[error("backend activation socket path must be absolute: {}", path.display())]
    RelativeSocketPath { path: PathBuf },
    #[error("backend activation socket path must not be empty")]
    EmptySocketPath,
    #[error("backend service template must be a fixed name ending in @.service: {0:?}")]
    InvalidServiceTemplate(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_fixed_absolute_endpoints() {
        let endpoint = BackendEndpoint::new(
            "mock",
            "/run/user/1000/pronk/backends/mock.sock",
            "pronk-backend-mock@.service",
        )
        .unwrap();
        assert_eq!(endpoint.backend_id(), "mock");
        assert_eq!(endpoint.service_template(), "pronk-backend-mock@.service");

        assert!(matches!(
            BackendEndpoint::new("mock", "relative.sock", "pronk-backend-mock@.service"),
            Err(EndpointError::RelativeSocketPath { .. })
        ));
        assert!(matches!(
            BackendEndpoint::new(
                "Mock Backend",
                "/run/user/1000/mock.sock",
                "pronk-backend-mock@.service"
            ),
            Err(EndpointError::InvalidBackendId(_))
        ));
        assert!(matches!(
            BackendEndpoint::new("mock", "/run/user/1000/mock.sock", "arbitrary.service"),
            Err(EndpointError::InvalidServiceTemplate(_))
        ));
    }
}
