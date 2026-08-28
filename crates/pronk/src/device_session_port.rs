//! Application-owned lifetime boundary for one prepared Device session.

use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::OwnedFd;

use async_trait::async_trait;
use thiserror::Error;

use crate::device_control_port::{DeviceControlError, DeviceControlOperation};
use crate::display_state::RoutedMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMediaKind {
    Video,
    Audio,
}

/// Exact PipeWire object a backend endpoint is authorized to consume.
///
/// These are application-owned values. The private-D-Bus adapter is
/// responsible for translating and validating its protocol representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMediaTarget {
    pub kind: DeviceMediaKind,
    pub node_name: String,
    pub object_serial: NonZeroU64,
    pub session_id: String,
    pub device_instance: String,
    pub connector_id: NonZeroU32,
    pub output_index: u32,
    pub media_generation: NonZeroU64,
    pub caps: String,
}

/// One fresh, untouched PipeWire connection paired with its sole target.
#[derive(Debug)]
pub struct DeviceMediaEndpoint {
    pub remote: OwnedFd,
    pub target: DeviceMediaTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMediaConfiguration {
    pub video_profile_id: String,
    pub audio_profile_id: Option<String>,
    pub mode: RoutedMode,
    pub video_bitrate: NonZeroU64,
}

/// Complete authority transfer for one immutable media generation.
#[derive(Debug)]
pub struct DeviceMediaSetup {
    pub media_generation: NonZeroU64,
    pub endpoints: Vec<DeviceMediaEndpoint>,
    pub configuration: DeviceMediaConfiguration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMediaSuspendReason {
    OutputDisabled,
    ModeChanged,
    DeviceUnavailable,
    SessionInactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMediaStopReason {
    OutputDisabled,
    ModeChanged,
    DisplayRemoved,
    BackendShutdown,
    TransportFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceSessionStopReason {
    DisplayRemoved,
    DaemonShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSessionEvent {
    pub session_generation: NonZeroU64,
    pub error: String,
}

/// Passive terminal-event source paired with one concrete Device session.
#[async_trait]
pub trait DeviceSessionEventPort: fmt::Debug + Send + 'static {
    async fn next_event(&mut self) -> Option<DeviceSessionEvent>;

    async fn shutdown(self: Box<Self>);
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct DeviceSessionError(String);

impl DeviceSessionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Operations the display lifecycle currently requires from a prepared
/// Device session.
#[async_trait]
pub trait DeviceSessionPort: fmt::Debug + Send + 'static {
    async fn transmit_control(
        &mut self,
        _operation: DeviceControlOperation,
    ) -> Result<(), DeviceControlError> {
        Err(DeviceControlError::new(
            "Device session does not support control operations",
        ))
    }

    async fn configure_media(&mut self, setup: DeviceMediaSetup) -> Result<(), DeviceSessionError>;

    async fn start_media(&mut self, media_generation: NonZeroU64)
        -> Result<(), DeviceSessionError>;

    async fn suspend_media(
        &mut self,
        media_generation: NonZeroU64,
        reason: DeviceMediaSuspendReason,
    ) -> Result<(), DeviceSessionError>;

    async fn resume_media(
        &mut self,
        media_generation: NonZeroU64,
    ) -> Result<(), DeviceSessionError>;

    async fn stop_media(
        &mut self,
        media_generation: NonZeroU64,
        reason: DeviceMediaStopReason,
    ) -> Result<(), DeviceSessionError>;

    async fn stop(
        self: Box<Self>,
        reason: DeviceSessionStopReason,
    ) -> Result<(), DeviceSessionError>;
}
