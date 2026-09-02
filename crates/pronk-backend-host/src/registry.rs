use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use nix::unistd::Uid;
use pronk_backend_protocol::PROTOCOL_MAJOR;
use pronk_userns::is_host_root_owner;
use serde::Deserialize;
use thiserror::Error;

use crate::{BackendEndpoint, EndpointError};

pub const INSTALLED_BACKEND_REGISTRY_DIR: &str = "/usr/lib/pronk/backends.d";
pub const SYSTEM_BACKEND_RUNTIME_DIR: &str = "/run";
pub const MAX_INSTALLED_BACKENDS: usize = 16;
pub const MAX_BACKEND_DEFINITION_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBackend {
    endpoint: BackendEndpoint,
    protocol_major: u16,
}

impl InstalledBackend {
    pub fn endpoint(&self) -> &BackendEndpoint {
        &self.endpoint
    }

    pub fn protocol_major(&self) -> u16 {
        self.protocol_major
    }
}

#[derive(Debug, Clone, Default)]
pub struct BackendRegistry {
    backends: BTreeMap<String, InstalledBackend>,
}

impl BackendRegistry {
    pub fn load_installed(runtime_directory: &Path) -> Result<Self, BackendRegistryError> {
        validate_runtime_directory(runtime_directory)?;
        Self::load_root_owned_directory(
            Path::new(INSTALLED_BACKEND_REGISTRY_DIR),
            runtime_directory,
        )
    }

    pub fn get(&self, backend_id: &str) -> Option<&InstalledBackend> {
        self.backends.get(backend_id)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &InstalledBackend)> {
        self.backends
            .iter()
            .map(|(backend_id, backend)| (backend_id.as_str(), backend))
    }

    pub fn len(&self) -> usize {
        self.backends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    fn load_root_owned_directory(
        registry_directory: &Path,
        runtime_directory: &Path,
    ) -> Result<Self, BackendRegistryError> {
        validate_root_owned_directory(registry_directory)?;
        let entries =
            fs::read_dir(registry_directory).map_err(|source| BackendRegistryError::Io {
                operation: "read registry directory",
                path: registry_directory.into(),
                source,
            })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| BackendRegistryError::Io {
                operation: "read registry entry",
                path: registry_directory.into(),
                source,
            })?;
            let path = entry.path();
            if path.extension() != Some(OsStr::new("toml")) {
                continue;
            }
            paths.push(path);
        }
        paths.sort();
        if paths.len() > MAX_INSTALLED_BACKENDS {
            return Err(BackendRegistryError::TooManyBackends(paths.len()));
        }

        let mut definitions = Vec::with_capacity(paths.len());
        for path in paths {
            validate_root_owned_file(&path)?;
            let metadata = fs::metadata(&path).map_err(|source| BackendRegistryError::Io {
                operation: "inspect backend definition",
                path: path.clone(),
                source,
            })?;
            if metadata.len() > MAX_BACKEND_DEFINITION_BYTES {
                return Err(BackendRegistryError::DefinitionTooLarge {
                    path,
                    size: metadata.len(),
                });
            }
            let contents =
                fs::read_to_string(&path).map_err(|source| BackendRegistryError::Io {
                    operation: "read backend definition",
                    path: path.clone(),
                    source,
                })?;
            definitions.push((path, contents));
        }
        Self::from_documents(runtime_directory, definitions)
    }

    fn from_documents(
        runtime_directory: &Path,
        definitions: Vec<(PathBuf, String)>,
    ) -> Result<Self, BackendRegistryError> {
        if !runtime_directory.is_absolute() {
            return Err(BackendRegistryError::InvalidRuntimeDirectory(
                runtime_directory.into(),
            ));
        }
        if definitions.len() > MAX_INSTALLED_BACKENDS {
            return Err(BackendRegistryError::TooManyBackends(definitions.len()));
        }

        let mut backends = BTreeMap::new();
        let mut socket_paths = HashSet::new();
        let mut socket_units = HashSet::new();
        let mut service_templates = HashSet::new();
        for (source, document) in definitions {
            let definition: BackendDefinition =
                toml::from_str(&document).map_err(|error| BackendRegistryError::Parse {
                    path: source.clone(),
                    error: error.to_string(),
                })?;
            let installed = definition.into_installed(runtime_directory, &source)?;
            let backend_id = installed.endpoint.backend_id().to_owned();
            if backends.contains_key(&backend_id) {
                return Err(BackendRegistryError::DuplicateBackendId(backend_id));
            }
            let socket_path = installed.endpoint.socket_path().to_owned();
            if !socket_paths.insert(socket_path.clone()) {
                return Err(BackendRegistryError::DuplicateSocketPath(socket_path));
            }
            let socket_unit = installed.endpoint.socket_unit();
            if !socket_units.insert(socket_unit.clone()) {
                return Err(BackendRegistryError::DuplicateSocketUnit(socket_unit));
            }
            let service_template = installed.endpoint.service_template().to_owned();
            if !service_templates.insert(service_template.clone()) {
                return Err(BackendRegistryError::DuplicateServiceTemplate(
                    service_template,
                ));
            }
            backends.insert(backend_id, installed);
        }
        Ok(Self { backends })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendDefinition {
    format_version: u16,
    backend_id: String,
    runtime_socket: String,
    socket_unit: String,
    service_template: String,
    protocol_major: u16,
}

impl BackendDefinition {
    fn into_installed(
        self,
        runtime_directory: &Path,
        source: &Path,
    ) -> Result<InstalledBackend, BackendRegistryError> {
        if self.format_version != 1 {
            return Err(BackendRegistryError::UnsupportedFormat {
                path: source.into(),
                version: self.format_version,
            });
        }
        if self.protocol_major != PROTOCOL_MAJOR {
            return Err(BackendRegistryError::UnsupportedProtocolMajor {
                path: source.into(),
                version: self.protocol_major,
            });
        }
        let relative_socket = Path::new(&self.runtime_socket);
        if self.runtime_socket.len() > 256
            || relative_socket.as_os_str().is_empty()
            || relative_socket.is_absolute()
            || !relative_socket
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(BackendRegistryError::InvalidRuntimeSocket {
                path: source.into(),
                socket: self.runtime_socket,
            });
        }
        let socket_path = runtime_directory.join(relative_socket);
        let endpoint = BackendEndpoint::new(self.backend_id, socket_path, self.service_template)
            .map_err(|error| BackendRegistryError::InvalidEndpoint {
                path: source.into(),
                error,
            })?;
        let expected_socket_unit = endpoint.socket_unit();
        if self.socket_unit != expected_socket_unit {
            return Err(BackendRegistryError::SocketUnitMismatch {
                path: source.into(),
                expected: expected_socket_unit,
                actual: self.socket_unit,
            });
        }
        Ok(InstalledBackend {
            endpoint,
            protocol_major: self.protocol_major,
        })
    }
}

fn validate_runtime_directory(path: &Path) -> Result<(), BackendRegistryError> {
    let effective_uid = Uid::effective();
    let user_runtime = PathBuf::from(format!("/run/user/{}", effective_uid.as_raw()));
    let is_user_runtime = path == user_runtime;
    let is_system_runtime = path == Path::new(SYSTEM_BACKEND_RUNTIME_DIR);
    if !is_user_runtime && !is_system_runtime {
        return Err(BackendRegistryError::InvalidRuntimeDirectory(path.into()));
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| BackendRegistryError::Io {
        operation: "inspect runtime directory",
        path: path.into(),
        source,
    })?;
    let trusted_owner = if is_system_runtime {
        is_host_root_owner(metadata.uid())
    } else {
        metadata.uid() == effective_uid.as_raw()
    };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !trusted_owner
        || metadata.mode() & 0o022 != 0
    {
        return Err(BackendRegistryError::UntrustedRuntimeDirectory(path.into()));
    }
    Ok(())
}

fn validate_root_owned_directory(path: &Path) -> Result<(), BackendRegistryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| BackendRegistryError::Io {
        operation: "inspect registry directory",
        path: path.into(),
        source,
    })?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !is_host_root_owner(metadata.uid())
        || metadata.mode() & 0o022 != 0
    {
        return Err(BackendRegistryError::UntrustedRegistryDirectory(
            path.into(),
        ));
    }
    Ok(())
}

fn validate_root_owned_file(path: &Path) -> Result<(), BackendRegistryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| BackendRegistryError::Io {
        operation: "inspect backend definition",
        path: path.into(),
        source,
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || !is_host_root_owner(metadata.uid())
        || metadata.mode() & 0o022 != 0
    {
        return Err(BackendRegistryError::UntrustedDefinition(path.into()));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum BackendRegistryError {
    #[error("backend runtime directory must be an absolute trusted directory: {}", .0.display())]
    InvalidRuntimeDirectory(PathBuf),
    #[error("backend runtime directory is not owned securely by the effective user: {}", .0.display())]
    UntrustedRuntimeDirectory(PathBuf),
    #[error("backend registry directory is not a root-owned non-writable directory: {}", .0.display())]
    UntrustedRegistryDirectory(PathBuf),
    #[error("backend definition is not a root-owned non-writable regular file: {}", .0.display())]
    UntrustedDefinition(PathBuf),
    #[error("{operation} {}: {source}", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("installed backend count {0} exceeds {MAX_INSTALLED_BACKENDS}")]
    TooManyBackends(usize),
    #[error("backend definition {} has {size} bytes, exceeding {MAX_BACKEND_DEFINITION_BYTES}", path.display())]
    DefinitionTooLarge { path: PathBuf, size: u64 },
    #[error("cannot parse backend definition {}: {error}", path.display())]
    Parse { path: PathBuf, error: String },
    #[error("backend definition {} uses unsupported format version {version}", path.display())]
    UnsupportedFormat { path: PathBuf, version: u16 },
    #[error("backend definition {} uses unsupported protocol major {version}", path.display())]
    UnsupportedProtocolMajor { path: PathBuf, version: u16 },
    #[error("backend definition {} has invalid relative runtime socket {socket:?}", path.display())]
    InvalidRuntimeSocket { path: PathBuf, socket: String },
    #[error("backend definition {} has invalid endpoint: {error}", path.display())]
    InvalidEndpoint { path: PathBuf, error: EndpointError },
    #[error("backend definition {} names socket unit {actual:?}, expected {expected:?}", path.display())]
    SocketUnitMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("backend ID {0:?} is defined more than once")]
    DuplicateBackendId(String),
    #[error("backend activation socket {} is defined more than once", .0.display())]
    DuplicateSocketPath(PathBuf),
    #[error("backend socket unit {0:?} is defined more than once")]
    DuplicateSocketUnit(String),
    #[error("backend service template {0:?} is defined more than once")]
    DuplicateServiceTemplate(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCK: &str = r#"
format_version = 1
backend_id = "mock"
runtime_socket = "pronk/backends/mock.sock"
socket_unit = "pronk-backend-mock.socket"
service_template = "pronk-backend-mock@.service"
protocol_major = 1
"#;

    fn registry(documents: &[(&str, &str)]) -> Result<BackendRegistry, BackendRegistryError> {
        BackendRegistry::from_documents(
            Path::new("/run/user/1000"),
            documents
                .iter()
                .map(|(name, document)| (PathBuf::from(name), (*document).into()))
                .collect(),
        )
    }

    #[test]
    fn resolves_only_a_fixed_relative_runtime_socket() {
        let registry = registry(&[("mock.toml", MOCK)]).unwrap();
        assert_eq!(registry.len(), 1);
        let installed = registry.get("mock").unwrap();
        assert_eq!(installed.protocol_major(), PROTOCOL_MAJOR);
        assert_eq!(
            installed.endpoint().socket_path(),
            Path::new("/run/user/1000/pronk/backends/mock.sock")
        );
        assert_eq!(
            installed.endpoint().service_template(),
            "pronk-backend-mock@.service"
        );
        assert!(registry.get("caller-selected").is_none());
    }

    #[test]
    fn resolves_the_system_socket_below_run() {
        let registry = BackendRegistry::from_documents(
            Path::new(SYSTEM_BACKEND_RUNTIME_DIR),
            vec![(PathBuf::from("mock.toml"), MOCK.into())],
        )
        .unwrap();
        assert_eq!(
            registry.get("mock").unwrap().endpoint().socket_path(),
            Path::new("/run/pronk/backends/mock.sock")
        );
    }

    #[test]
    fn rejects_path_escape_unit_mismatch_and_unknown_fields() {
        assert!(matches!(
            registry(&[(
                "escape.toml",
                &MOCK.replace("pronk/backends/mock.sock", "../attacker-controlled.sock")
            )]),
            Err(BackendRegistryError::InvalidRuntimeSocket { .. })
        ));
        assert!(matches!(
            registry(&[(
                "unit.toml",
                &MOCK.replace("pronk-backend-mock.socket", "other.socket")
            )]),
            Err(BackendRegistryError::SocketUnitMismatch { .. })
        ));
        assert!(matches!(
            registry(&[(
                "unknown.toml",
                &format!("{MOCK}\nexecutable = \"/tmp/pwn\"")
            )]),
            Err(BackendRegistryError::Parse { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_authority_endpoints() {
        let duplicate_id = MOCK.replace(
            "runtime_socket = \"pronk/backends/mock.sock\"",
            "runtime_socket = \"pronk/backends/other.sock\"",
        );
        assert!(matches!(
            registry(&[("one.toml", MOCK), ("two.toml", &duplicate_id)]),
            Err(BackendRegistryError::DuplicateBackendId(_))
        ));

        let duplicate_socket = MOCK
            .replace("backend_id = \"mock\"", "backend_id = \"other\"")
            .replace(
                "socket_unit = \"pronk-backend-mock.socket\"",
                "socket_unit = \"pronk-backend-other.socket\"",
            )
            .replace(
                "service_template = \"pronk-backend-mock@.service\"",
                "service_template = \"pronk-backend-other@.service\"",
            );
        assert!(matches!(
            registry(&[("one.toml", MOCK), ("two.toml", &duplicate_socket)]),
            Err(BackendRegistryError::DuplicateSocketPath(_))
        ));
    }
}
