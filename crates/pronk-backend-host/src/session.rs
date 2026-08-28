//! Generation-bound control of one backend session.

use std::num::NonZeroU64;
use std::time::Duration;

use futures_util::StreamExt;
use pronk_backend_protocol::{
    session_object_path, validate_error_text, validate_media_configuration, Backend1Proxy,
    BackendSession1Proxy, ControlOperation, DeviceAvailability, DeviceCapabilities,
    MediaConfiguration, PipeWireTarget, PreparationRequest, SessionOptions, SessionStatistics,
    StopReason, SuspendReason, Validate, MAX_DEVICE_TEXT_BYTES,
};
use thiserror::Error;
use tokio::time::timeout;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use zbus::Connection;

use crate::DeviceInventorySnapshot;

pub const BACKEND_SESSION_CREATE_TIMEOUT: Duration = Duration::from_secs(5);
pub const BACKEND_SESSION_PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
pub const BACKEND_SESSION_MEDIA_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
pub const BACKEND_SESSION_MEDIA_STOP_TIMEOUT: Duration = Duration::from_secs(5);
pub const BACKEND_SESSION_CONTROL_TIMEOUT: Duration = Duration::from_millis(1_500);
pub const BACKEND_SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSessionRequest {
    session_id: String,
    device_id: String,
    options: SessionOptions,
    expected_path: OwnedObjectPath,
}

impl BackendSessionRequest {
    pub fn new(
        session_id: impl Into<String>,
        device_id: impl Into<String>,
        options: SessionOptions,
    ) -> Result<Self, BackendSessionError> {
        let session_id = session_id.into();
        let device_id = device_id.into();
        options
            .validate()
            .map_err(|error| BackendSessionError::InvalidRequest(error.to_string()))?;
        let expected_path = session_object_path(&session_id, options.session_generation)
            .map_err(|error| BackendSessionError::InvalidRequest(error.to_string()))?;
        if device_id.is_empty()
            || device_id.len() > MAX_DEVICE_TEXT_BYTES
            || device_id.chars().any(char::is_control)
        {
            return Err(BackendSessionError::InvalidRequest(
                "device ID is empty, too long, or contains a control character".into(),
            ));
        }
        Ok(Self {
            session_id,
            device_id,
            options,
            expected_path,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn options(&self) -> &SessionOptions {
        &self.options
    }
}

#[derive(Debug)]
pub struct BackendSessionHandle {
    connection: Connection,
    backend_id: String,
    session_id: String,
    device_id: String,
    object_path: OwnedObjectPath,
    connection_generation: u64,
    discovery_generation: u64,
    session_generation: u64,
}

impl BackendSessionHandle {
    pub(crate) fn connection_for_monitor(&self) -> &Connection {
        &self.connection
    }
    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn object_path(&self) -> &OwnedObjectPath {
        &self.object_path
    }

    pub fn connection_generation(&self) -> u64 {
        self.connection_generation
    }

    pub fn discovery_generation(&self) -> u64 {
        self.discovery_generation
    }

    pub fn session_generation(&self) -> u64 {
        self.session_generation
    }

    pub async fn prepare(
        &self,
        request: PreparationRequest,
    ) -> Result<DeviceCapabilities, BackendSessionError> {
        request
            .validate()
            .map_err(|error| BackendSessionError::InvalidRequest(error.to_string()))?;
        let expected_generation = request.preparation_generation;
        let offer = request.clone();
        let proxy = BackendSession1Proxy::builder(&self.connection)
            .path(self.object_path.clone())
            .map_err(BackendSessionError::Protocol)?
            .build()
            .await
            .map_err(BackendSessionError::Protocol)?;
        let capabilities = timeout(BACKEND_SESSION_PREPARE_TIMEOUT, proxy.prepare(request))
            .await
            .map_err(|_| BackendSessionError::MethodTimeout("Prepare"))?
            .map_err(BackendSessionError::Protocol)?;
        capabilities
            .validate()
            .map_err(|error| BackendSessionError::InvalidCapabilities(error.to_string()))?;
        if capabilities.preparation_generation != expected_generation {
            return Err(BackendSessionError::StalePreparationGeneration {
                expected: expected_generation,
                actual: capabilities.preparation_generation,
            });
        }
        validate_capabilities_against_offer(&offer, &capabilities)?;
        Ok(capabilities)
    }

    /// Transfer fresh PipeWire remotes and their exact generation-bound
    /// targets to the backend.
    ///
    /// The descriptors are consumed even when the call fails or times out.
    /// A caller must therefore treat any error as an ambiguous transfer and
    /// clean up this generation rather than retrying with the same remotes.
    pub async fn configure_media(
        &self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: NonZeroU64,
    ) -> Result<(), BackendSessionError> {
        validate_media_configuration(
            remotes.len(),
            &targets,
            &configuration,
            media_generation.get(),
        )
        .map_err(|error| BackendSessionError::InvalidRequest(error.to_string()))?;
        let proxy = self.proxy().await?;
        timeout(
            BACKEND_SESSION_MEDIA_CONTROL_TIMEOUT,
            proxy.configure_media(remotes, targets, configuration, media_generation.get()),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("ConfigureMedia"))?
        .map_err(BackendSessionError::Protocol)
    }

    pub async fn start_media(
        &self,
        media_generation: NonZeroU64,
    ) -> Result<(), BackendSessionError> {
        let proxy = self.proxy().await?;
        timeout(
            BACKEND_SESSION_MEDIA_CONTROL_TIMEOUT,
            proxy.start(media_generation.get()),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("Start"))?
        .map_err(BackendSessionError::Protocol)
    }

    pub async fn suspend_media(&self, reason: SuspendReason) -> Result<(), BackendSessionError> {
        let proxy = self.proxy().await?;
        timeout(BACKEND_SESSION_MEDIA_CONTROL_TIMEOUT, proxy.suspend(reason))
            .await
            .map_err(|_| BackendSessionError::MethodTimeout("Suspend"))?
            .map_err(BackendSessionError::Protocol)
    }

    pub async fn resume_media(
        &self,
        media_generation: NonZeroU64,
    ) -> Result<(), BackendSessionError> {
        let proxy = self.proxy().await?;
        timeout(
            BACKEND_SESSION_MEDIA_CONTROL_TIMEOUT,
            proxy.resume(media_generation.get()),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("Resume"))?
        .map_err(BackendSessionError::Protocol)
    }

    pub async fn stop_media(
        &self,
        media_generation: NonZeroU64,
        reason: StopReason,
    ) -> Result<(), BackendSessionError> {
        let proxy = self.proxy().await?;
        timeout(
            BACKEND_SESSION_MEDIA_STOP_TIMEOUT,
            proxy.stop_media(media_generation.get(), reason),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("StopMedia"))?
        .map_err(BackendSessionError::Protocol)
    }

    async fn proxy(&self) -> Result<BackendSession1Proxy<'_>, BackendSessionError> {
        BackendSession1Proxy::builder(&self.connection)
            .path(self.object_path.clone())
            .map_err(BackendSessionError::Protocol)?
            .build()
            .await
            .map_err(BackendSessionError::Protocol)
    }

    /// Submit one normalized Device control operation and wait for its exact
    /// generation/operation completion signal.
    ///
    /// Subscription precedes the method call so even an immediately completed
    /// backend operation cannot race the listener.
    pub async fn transmit_control(
        &self,
        operation: ControlOperation,
    ) -> Result<(), BackendSessionError> {
        operation
            .validate()
            .map_err(|error| BackendSessionError::InvalidRequest(error.to_string()))?;
        if operation.session_generation != self.session_generation {
            return Err(BackendSessionError::InvalidRequest(format!(
                "control session generation {} differs from {}",
                operation.session_generation, self.session_generation
            )));
        }

        let proxy = self.proxy().await?;
        let mut completions = timeout(
            BACKEND_SESSION_CONTROL_TIMEOUT,
            proxy.receive_control_completed(),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("SubscribeControlCompleted"))?
        .map_err(BackendSessionError::Protocol)?;
        let operation_id = timeout(
            BACKEND_SESSION_CONTROL_TIMEOUT,
            proxy.transmit_control(operation),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("TransmitControl"))?
        .map_err(BackendSessionError::Protocol)?;
        if operation_id == 0 {
            return Err(BackendSessionError::InvalidControlCompletion(
                "operation ID is zero".into(),
            ));
        }

        let signal = timeout(BACKEND_SESSION_CONTROL_TIMEOUT, completions.next())
            .await
            .map_err(|_| BackendSessionError::MethodTimeout("ControlCompleted"))?
            .ok_or(BackendSessionError::ControlCompletionStreamClosed)?;
        let args = signal.args().map_err(BackendSessionError::Protocol)?;
        if *args.session_generation() != self.session_generation {
            return Err(BackendSessionError::InvalidControlCompletion(format!(
                "session generation {} differs from {}",
                args.session_generation(),
                self.session_generation
            )));
        }
        if *args.operation_id() != operation_id {
            return Err(BackendSessionError::InvalidControlCompletion(format!(
                "operation ID {} differs from {operation_id}",
                args.operation_id()
            )));
        }
        let succeeded = *args.succeeded();
        let error_text = args.error_text();
        if succeeded {
            if !error_text.is_empty() {
                return Err(BackendSessionError::InvalidControlCompletion(
                    "successful completion has error text".into(),
                ));
            }
            Ok(())
        } else {
            validate_error_text(error_text).map_err(|error| {
                BackendSessionError::InvalidControlCompletion(error.to_string())
            })?;
            Err(BackendSessionError::ControlFailed(error_text.clone()))
        }
    }

    pub async fn get_statistics(
        &self,
        media_generation: NonZeroU64,
    ) -> Result<SessionStatistics, BackendSessionError> {
        let proxy = self.proxy().await?;
        let statistics = timeout(
            BACKEND_SESSION_MEDIA_CONTROL_TIMEOUT,
            proxy.get_statistics(),
        )
        .await
        .map_err(|_| BackendSessionError::MethodTimeout("GetStatistics"))?
        .map_err(BackendSessionError::Protocol)?;
        statistics
            .validate()
            .map_err(|error| BackendSessionError::InvalidStatistics(error.to_string()))?;
        if statistics.session_generation != self.session_generation {
            return Err(BackendSessionError::StaleStatisticsGeneration {
                kind: "session",
                expected: self.session_generation,
                actual: statistics.session_generation,
            });
        }
        if statistics.media_generation != media_generation.get() {
            return Err(BackendSessionError::StaleStatisticsGeneration {
                kind: "media",
                expected: media_generation.get(),
                actual: statistics.media_generation,
            });
        }
        Ok(statistics)
    }

    pub async fn stop(self, reason: StopReason) -> Result<(), BackendSessionError> {
        let proxy = BackendSession1Proxy::builder(&self.connection)
            .path(self.object_path)
            .map_err(BackendSessionError::Protocol)?
            .build()
            .await
            .map_err(BackendSessionError::Protocol)?;
        timeout(BACKEND_SESSION_STOP_TIMEOUT, proxy.stop(reason))
            .await
            .map_err(|_| BackendSessionError::MethodTimeout("Stop"))?
            .map_err(BackendSessionError::Protocol)
    }
}

fn validate_capabilities_against_offer(
    offer: &PreparationRequest,
    capabilities: &DeviceCapabilities,
) -> Result<(), BackendSessionError> {
    if capabilities.features & !offer.requested_features != 0 {
        return Err(BackendSessionError::CapabilitiesOutsideOffer(
            "feature bits",
        ));
    }
    if capabilities
        .modes
        .iter()
        .any(|mode| !offer.candidate_modes.contains(mode))
    {
        return Err(BackendSessionError::CapabilitiesOutsideOffer(
            "display mode",
        ));
    }
    for returned in &capabilities.video_profiles {
        let Some(offered) = offer
            .video_profiles
            .iter()
            .find(|offered| offered.profile_id == returned.profile_id)
        else {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "video profile ID",
            ));
        };
        if returned.codec != offered.codec
            || returned.max_width > offered.max_width
            || returned.max_height > offered.max_height
            || returned.max_refresh_millihz > offered.max_refresh_millihz
        {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "video profile limits",
            ));
        }
    }
    for returned in &capabilities.audio_profiles {
        let Some(offered) = offer
            .audio_profiles
            .iter()
            .find(|offered| offered.profile_id == returned.profile_id)
        else {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "audio profile ID",
            ));
        };
        if returned.codec != offered.codec
            || returned.max_channels > offered.max_channels
            || returned
                .sample_rates
                .iter()
                .any(|rate| !offered.sample_rates.contains(rate))
        {
            return Err(BackendSessionError::CapabilitiesOutsideOffer(
                "audio profile limits",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn create_connected_session(
    connection: &Connection,
    backend_id: &str,
    active_connection_generation: u64,
    inventory: &DeviceInventorySnapshot,
    request: BackendSessionRequest,
) -> Result<BackendSessionHandle, BackendSessionError> {
    if request.options.connection_generation != active_connection_generation {
        return Err(BackendSessionError::StaleConnectionGeneration {
            expected: active_connection_generation,
            actual: request.options.connection_generation,
        });
    }
    if request.options.discovery_generation != inventory.discovery_generation {
        return Err(BackendSessionError::StaleDiscoveryGeneration {
            expected: inventory.discovery_generation,
            actual: request.options.discovery_generation,
        });
    }
    if !inventory.devices.iter().any(|device| {
        device.device_id == request.device_id
            && device.availability == DeviceAvailability::Available
    }) {
        return Err(BackendSessionError::DeviceUnavailable(
            request.device_id.clone(),
        ));
    }

    let proxy = Backend1Proxy::new(connection)
        .await
        .map_err(BackendSessionError::Protocol)?;
    let returned_path = timeout(
        BACKEND_SESSION_CREATE_TIMEOUT,
        proxy.create_session(
            request.session_id.clone(),
            request.device_id.clone(),
            request.options.clone(),
        ),
    )
    .await
    .map_err(|_| BackendSessionError::MethodTimeout("CreateSession"))?
    .map_err(BackendSessionError::Protocol)?;
    if returned_path != request.expected_path {
        return Err(BackendSessionError::UnexpectedObjectPath {
            expected: request.expected_path.to_string(),
            actual: returned_path.to_string(),
        });
    }

    Ok(BackendSessionHandle {
        connection: connection.clone(),
        backend_id: backend_id.into(),
        session_id: request.session_id,
        device_id: request.device_id,
        object_path: returned_path,
        connection_generation: request.options.connection_generation,
        discovery_generation: request.options.discovery_generation,
        session_generation: request.options.session_generation,
    })
}

#[derive(Debug, Error)]
pub enum BackendSessionError {
    #[error("invalid backend session request: {0}")]
    InvalidRequest(String),
    #[error("backend is not connected")]
    BackendUnavailable,
    #[error("backend supervisor has stopped")]
    SupervisorStopped,
    #[error("backend session monitor stopped during startup")]
    MonitorStopped,
    #[error("connection generation {actual} is stale; active generation is {expected}")]
    StaleConnectionGeneration { expected: u64, actual: u64 },
    #[error("discovery generation {actual} is stale; active generation is {expected}")]
    StaleDiscoveryGeneration { expected: u64, actual: u64 },
    #[error("device {0:?} is not available in the active backend inventory")]
    DeviceUnavailable(String),
    #[error("backend session method {0} timed out")]
    MethodTimeout(&'static str),
    #[error("backend session protocol failed: {0}")]
    Protocol(zbus::Error),
    #[error("backend returned session object {actual:?}; expected {expected:?}")]
    UnexpectedObjectPath { expected: String, actual: String },
    #[error("backend returned invalid device capabilities: {0}")]
    InvalidCapabilities(String),
    #[error("backend returned preparation generation {actual}; expected {expected}")]
    StalePreparationGeneration { expected: u64, actual: u64 },
    #[error("backend returned {0} outside the bounded preparation offer")]
    CapabilitiesOutsideOffer(&'static str),
    #[error("backend returned invalid session statistics: {0}")]
    InvalidStatistics(String),
    #[error("backend returned invalid control completion: {0}")]
    InvalidControlCompletion(String),
    #[error("backend control operation failed: {0}")]
    ControlFailed(String),
    #[error("backend control-completion signal stream closed")]
    ControlCompletionStreamClosed,
    #[error(
        "backend returned {kind} statistics generation {actual}; expected generation {expected}"
    )]
    StaleStatisticsGeneration {
        kind: &'static str,
        expected: u64,
        actual: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pronk_backend_protocol::{
        AudioProfile, DisplayIdentity, DisplayMode, IdentitySource, VideoProfile,
        SESSION_FEATURE_AUDIO,
    };

    const SESSION_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    fn options() -> SessionOptions {
        SessionOptions {
            connection_generation: 1,
            discovery_generation: 2,
            session_generation: 3,
            requested_features: 0,
        }
    }

    #[test]
    fn request_validates_identity_and_generations_before_ipc() {
        let request = BackendSessionRequest::new(SESSION_ID, "living-room", options()).unwrap();
        assert_eq!(request.session_id(), SESSION_ID);
        assert_eq!(request.device_id(), "living-room");
        assert_eq!(request.options(), &options());

        assert!(matches!(
            BackendSessionRequest::new("not-a-uuid", "living-room", options()),
            Err(BackendSessionError::InvalidRequest(_))
        ));
        assert!(matches!(
            BackendSessionRequest::new(SESSION_ID, "bad\ndevice", options()),
            Err(BackendSessionError::InvalidRequest(_))
        ));
        let mut zero_generation = options();
        zero_generation.session_generation = 0;
        assert!(matches!(
            BackendSessionRequest::new(SESSION_ID, "living-room", zero_generation),
            Err(BackendSessionError::InvalidRequest(_))
        ));
    }

    fn preparation() -> PreparationRequest {
        PreparationRequest {
            preparation_generation: 1,
            candidate_modes: vec![DisplayMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            }],
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 3840,
                max_height: 2160,
                max_refresh_millihz: 60_000,
            }],
            audio_profiles: vec![AudioProfile {
                profile_id: "opus-stereo".into(),
                codec: "opus".into(),
                max_channels: 2,
                sample_rates: vec![48_000],
            }],
            requested_features: SESSION_FEATURE_AUDIO,
        }
    }

    fn capabilities() -> DeviceCapabilities {
        let offer = preparation();
        DeviceCapabilities {
            preparation_generation: 1,
            display_identity: DisplayIdentity {
                manufacturer_name: Some("Pronk Project".into()),
                manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
                product_name: Some("Mock Display".into()),
                product_source: IdentitySource::AuthenticatedDeviceInfo,
                pnp_id: None,
            },
            modes: offer.candidate_modes,
            video_profiles: offer.video_profiles,
            audio_profiles: offer.audio_profiles,
            features: offer.requested_features,
        }
    }

    #[test]
    fn capabilities_may_narrow_but_never_expand_the_offer() {
        let offer = preparation();
        let capabilities = capabilities();
        validate_capabilities_against_offer(&offer, &capabilities).unwrap();

        let mut expanded = capabilities.clone();
        expanded.modes[0].width = 3840;
        assert!(matches!(
            validate_capabilities_against_offer(&offer, &expanded),
            Err(BackendSessionError::CapabilitiesOutsideOffer(
                "display mode"
            ))
        ));

        let mut expanded = capabilities.clone();
        expanded.video_profiles[0].max_width += 1;
        assert!(matches!(
            validate_capabilities_against_offer(&offer, &expanded),
            Err(BackendSessionError::CapabilitiesOutsideOffer(
                "video profile limits"
            ))
        ));

        let mut expanded = capabilities.clone();
        expanded.audio_profiles[0].sample_rates.push(44_100);
        assert!(matches!(
            validate_capabilities_against_offer(&offer, &expanded),
            Err(BackendSessionError::CapabilitiesOutsideOffer(
                "audio profile limits"
            ))
        ));

        let mut expanded = capabilities;
        expanded.features |= 1 << 1;
        assert!(matches!(
            validate_capabilities_against_offer(&offer, &expanded),
            Err(BackendSessionError::CapabilitiesOutsideOffer(
                "feature bits"
            ))
        ));
    }
}
