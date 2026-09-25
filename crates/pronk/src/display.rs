//! Cancellable transition from a pending output reservation to one attached
//! cast display.

mod setup;

use std::fmt::Write as _;
use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;

use nix::libc;
use pronk_backend_host::BackendSessionError;
use pronk_backend_protocol::{PreparationRequest, Validate};
use pronk_core::identity::PnpIdResolver;
use pronk_core::output::CastKmsOutputId;
use pronk_core::session::PinnedCallerProcess;
use pronk_dbus::{DeviceInfo, DeviceSelection, OperationErrorCode};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::device_recovery::DeviceSessionFactoryPort;
use crate::device_session_port::DeviceSessionEventPort;
use crate::display_media::CaptureSource;
use crate::display_state::{DisplayGrantState, DisplayRuntimeState};
use crate::kernel_display::AttachError;
use crate::kernel_display_port::KernelDisplayPort;
use crate::kernel_session::KernelSessionError;
use crate::kernel_session_provider::KernelSessionProvider;
use crate::manager::{
    ManagerHandle, ReserveDisplaySlotError, ReservedCastDisplaySlot, ResolveDeviceError,
};
use crate::media_session::MediaSessionDriver;
use crate::preparation::{PrepareCastDeviceError, PreparedCastDevice};
use crate::replaceable_device_session::DeviceSessionReplacementHandle;
use crate::slot::OutputReservationError;

use setup::{run_display_setup, DisplayReservation, DisplaySetupCaller, DisplaySetupContext};

#[cfg(test)]
use crate::display_media::DisplayMediaAccess;
#[cfg(test)]
use crate::kernel_display::{KernelDisplay, KernelDisplayConfig, DEFAULT_TOPOLOGY_POLL_INTERVAL};
#[cfg(test)]
use crate::kernel_session::KernelSession;
#[cfg(test)]
use nix::unistd::Uid;
#[cfg(test)]
use pronk_core::edid::{EdidMode, ValidatedEdid};
#[cfg(test)]
use setup::{attach_kernel_session, cleanup_attached_kernel_session, AttachedKernelSession};
#[cfg(test)]
use std::num::NonZeroU32;

const INITIAL_SESSION_GENERATION: u64 = 1;
const MAX_OPERATION_ERROR_BYTES: usize = 512;
const VIDEO_FRAME_RATE_NUMERATOR: u32 = 30;
const VIDEO_FRAME_RATE_DENOMINATOR: u32 = 1;

/// Capture selection and PipeWire runtime for the account running media services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRuntime {
    directory: PathBuf,
    server_uid: u32,
    capture_source: CaptureSource,
}

impl MediaRuntime {
    pub fn new(directory: PathBuf, server_uid: u32) -> Self {
        Self {
            directory,
            server_uid,
            capture_source: CaptureSource::default(),
        }
    }

    pub fn for_user(uid: u32) -> Self {
        Self::new(PathBuf::from(format!("/run/user/{uid}/pronk/media")), uid)
    }

    pub fn with_capture_source(mut self, capture_source: CaptureSource) -> Self {
        self.capture_source = capture_source;
        self
    }

    pub(crate) fn capture_source(&self) -> CaptureSource {
        self.capture_source
    }
}

pub(crate) struct DisplaySetupDependencies {
    kernel_session_provider: Arc<dyn KernelSessionProvider>,
    pnp_resolver: Arc<PnpIdResolver>,
    media_runtime: MediaRuntime,
    offer: PreparationRequest,
    audio_enabled: bool,
}

impl DisplaySetupDependencies {
    pub(crate) fn new(
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
        media_runtime: MediaRuntime,
        offer: PreparationRequest,
        audio_enabled: bool,
    ) -> Self {
        Self {
            kernel_session_provider,
            pnp_resolver,
            media_runtime,
            offer,
            audio_enabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CastDisplayId([u8; 16]);

impl CastDisplayId {
    pub fn generate() -> Result<Self, io::Error> {
        let mut bytes = [0_u8; 16];
        let mut filled = 0;
        while filled < bytes.len() {
            // SAFETY: the pointer names the unfilled part of `bytes` and
            // getrandom does not retain it.
            let count = unsafe {
                libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "getrandom returned no cast-display UUID bytes",
                ));
            }
            filled += count as usize;
        }
        // RFC 4122 variant, random version 4.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(bytes))
    }

    pub fn object_segment(self) -> String {
        let mut segment = String::with_capacity(32);
        for byte in self.0 {
            write!(&mut segment, "{byte:02x}").expect("writing to a String cannot fail");
        }
        segment
    }
}

impl std::fmt::Display for CastDisplayId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = self.0;
        write!(
            formatter,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            value[0],
            value[1],
            value[2],
            value[3],
            value[4],
            value[5],
            value[6],
            value[7],
            value[8],
            value[9],
            value[10],
            value[11],
            value[12],
            value[13],
            value[14],
            value[15],
        )
    }
}

impl std::str::FromStr for CastDisplayId {
    type Err = CastDisplayIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 36 {
            return Err(CastDisplayIdParseError);
        }
        let bytes = value.as_bytes();
        for index in [8, 13, 18, 23] {
            if bytes[index] != b'-' {
                return Err(CastDisplayIdParseError);
            }
        }
        let mut identity = [0_u8; 16];
        let mut input = 0;
        for output in &mut identity {
            while matches!(input, 8 | 13 | 18 | 23) {
                input += 1;
            }
            let high = hex_nibble(bytes[input]).ok_or(CastDisplayIdParseError)?;
            let low = hex_nibble(bytes[input + 1]).ok_or(CastDisplayIdParseError)?;
            *output = high << 4 | low;
            input += 2;
        }
        // Public IDs are always the random UUIDs generated by this daemon.
        if identity[6] >> 4 != 4 || identity[8] >> 6 != 2 {
            return Err(CastDisplayIdParseError);
        }
        Ok(Self(identity))
    }
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("cast-display ID is not a canonical random UUID")]
pub struct CastDisplayIdParseError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaySetupStage {
    Validating,
    Authorizing,
    PreparingDevice,
    Attaching,
    Added,
    Cancelled,
    Failed,
}

impl DisplaySetupStage {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Added | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaySetupSnapshot {
    pub display_id: CastDisplayId,
    pub stage: DisplaySetupStage,
    pub error_code: OperationErrorCode,
    pub error: Option<String>,
}

pub(crate) struct PendingDisplaySelection {
    pub selection: DeviceSelection,
    pub preferred_output: Option<CastKmsOutputId>,
}

#[derive(Debug)]
pub struct DisplaySetupOperation {
    handle: DisplaySetupHandle,
    task: Option<JoinHandle<Result<AddedCastDisplay, DisplaySetupError>>>,
}

/// Cloneable observation and cancellation capability for one manager-owned
/// setup operation.
///
/// Dropping this handle has no lifecycle effect. The manager owns the setup
/// task and every partially acquired resource until it reaches a terminal
/// state; cancellation is always explicit.
#[derive(Debug, Clone)]
pub struct DisplaySetupHandle {
    display_id: CastDisplayId,
    cancellation: CancellationToken,
    status: watch::Receiver<DisplaySetupSnapshot>,
}

impl DisplaySetupHandle {
    pub fn display_id(&self) -> CastDisplayId {
        self.display_id
    }

    pub fn snapshot(&self) -> DisplaySetupSnapshot {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<DisplaySetupSnapshot> {
        self.status.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl DisplaySetupOperation {
    pub fn spawn(
        slot: ReservedCastDisplaySlot,
        caller: PinnedCallerProcess,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
        media_runtime: MediaRuntime,
        offer: PreparationRequest,
        audio_enabled: bool,
    ) -> Result<Self, DisplaySetupStartError> {
        Self::spawn_with_caller(
            DisplayReservation::Ready(Box::new(slot)),
            DisplaySetupCaller::from(caller),
            DisplaySetupDependencies::new(
                kernel_session_provider,
                pnp_resolver,
                media_runtime,
                offer,
                audio_enabled,
            ),
            DisplaySetupStage::Authorizing,
        )
    }

    pub(crate) fn spawn_pending(
        manager: ManagerHandle,
        pending: PendingDisplaySelection,
        caller: PinnedCallerProcess,
        dependencies: DisplaySetupDependencies,
    ) -> Result<Self, DisplaySetupStartError> {
        Self::spawn_with_caller(
            DisplayReservation::Pending { manager, pending },
            DisplaySetupCaller::from(caller),
            dependencies,
            DisplaySetupStage::Validating,
        )
    }

    fn spawn_with_caller(
        reservation: DisplayReservation,
        caller: DisplaySetupCaller,
        dependencies: DisplaySetupDependencies,
        initial_stage: DisplaySetupStage,
    ) -> Result<Self, DisplaySetupStartError> {
        dependencies
            .offer
            .validate()
            .map_err(|error| DisplaySetupStartError::InvalidOffer(error.to_string()))?;
        let display_id = CastDisplayId::generate().map_err(DisplaySetupStartError::Identity)?;
        let cancellation = CancellationToken::new();
        let (status_tx, status) = watch::channel(DisplaySetupSnapshot {
            display_id,
            stage: initial_stage,
            error_code: OperationErrorCode::None,
            error: None,
        });
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            let result = run_display_setup(
                reservation,
                caller,
                DisplaySetupContext {
                    display_id,
                    kernel_session_provider: dependencies.kernel_session_provider,
                    pnp_resolver: dependencies.pnp_resolver,
                    media_runtime: dependencies.media_runtime,
                    offer: dependencies.offer,
                    audio_enabled: dependencies.audio_enabled,
                    cancellation: task_cancellation,
                    status: status_tx.clone(),
                },
            )
            .await;
            match &result {
                Ok(_) => set_status(
                    &status_tx,
                    DisplaySetupStage::Added,
                    OperationErrorCode::None,
                    None,
                ),
                Err(error @ (DisplaySetupError::Cancelled | DisplaySetupError::CallerExited)) => {
                    set_status(
                        &status_tx,
                        DisplaySetupStage::Cancelled,
                        error.operation_error_code(),
                        Some(error.to_string()),
                    );
                }
                Err(error) => {
                    set_status(
                        &status_tx,
                        DisplaySetupStage::Failed,
                        error.operation_error_code(),
                        Some(error.to_string()),
                    );
                }
            }
            result
        });
        Ok(Self {
            handle: DisplaySetupHandle {
                display_id,
                cancellation,
                status,
            },
            task: Some(task),
        })
    }

    pub fn display_id(&self) -> CastDisplayId {
        self.handle.display_id()
    }

    pub fn snapshot(&self) -> DisplaySetupSnapshot {
        self.handle.snapshot()
    }

    pub fn subscribe(&self) -> watch::Receiver<DisplaySetupSnapshot> {
        self.handle.subscribe()
    }

    pub fn handle(&self) -> DisplaySetupHandle {
        self.handle.clone()
    }

    pub fn cancel(&self) {
        self.handle.cancel();
    }

    pub async fn finish(mut self) -> Result<AddedCastDisplay, DisplaySetupOperationError> {
        let task = self
            .task
            .take()
            .ok_or(DisplaySetupOperationError::AlreadyFinished)?;
        task.await
            .map_err(DisplaySetupOperationError::Task)?
            .map_err(DisplaySetupOperationError::Setup)
    }
}

impl Drop for DisplaySetupOperation {
    fn drop(&mut self) {
        if self.task.is_some() {
            // The detached task retains every partially acquired resource and
            // follows its normal bounded cleanup path before exiting.
            self.handle.cancel();
        }
    }
}

/// Transfer object produced by setup and consumed exactly once by the
/// per-display slot actor.
///
/// It is not a second runtime display model: normal state mutation and
/// teardown begin only after ownership moves through `into_resources`.
#[derive(Debug)]
pub struct AddedCastDisplay {
    resources: Option<AddedCastDisplayResources>,
}

/// Bounded read-only projection of a manager-owned added display.
///
/// This is an internal control-plane snapshot. The public D-Bus adapter uses a
/// narrower protocol type and never exposes DRM node paths or kernel session IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedCastDisplaySnapshot {
    pub display_id: CastDisplayId,
    pub state_revision: u64,
    pub device: DeviceInfo,
    pub prepared: PreparedCastDevice,
    pub output: pronk_core::output::CastKmsOutput,
    pub kernel_session_id: NonZeroU64,
    pub grant_state: DisplayGrantState,
    pub runtime: DisplayRuntimeState,
}

#[derive(Debug)]
pub(crate) struct AddedCastDisplayResources {
    pub display_id: CastDisplayId,
    pub state_revision: u64,
    pub device: DeviceInfo,
    pub prepared: PreparedCastDevice,
    pub slot: ReservedCastDisplaySlot,
    pub media_driver: Box<dyn MediaSessionDriver>,
    pub recovery_factory: Box<dyn DeviceSessionFactoryPort>,
    pub session_replacement: DeviceSessionReplacementHandle,
    pub initial_session_generation: NonZeroU64,
    pub session_events: Box<dyn DeviceSessionEventPort>,
    pub kernel: Box<dyn KernelDisplayPort>,
}

impl AddedCastDisplay {
    fn resources(&self) -> &AddedCastDisplayResources {
        self.resources
            .as_ref()
            .expect("added display resources are present until transfer")
    }

    pub fn display_id(&self) -> CastDisplayId {
        self.resources().display_id
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.resources().device
    }

    pub(crate) fn update_device(&mut self, device: DeviceInfo) -> bool {
        let resources = self
            .resources
            .as_mut()
            .expect("added display resources are present until transfer");
        debug_assert_eq!(resources.device.backend_id, device.backend_id);
        debug_assert_eq!(resources.device.device_id, device.device_id);
        if resources.device == device {
            return false;
        }
        resources.state_revision = resources
            .state_revision
            .saturating_add(1)
            .max(device.device_revision);
        resources.device = device;
        true
    }

    pub(crate) fn into_resources(mut self) -> AddedCastDisplayResources {
        self.resources
            .take()
            .expect("added display resources are present until transfer")
    }
}

impl Drop for AddedCastDisplay {
    fn drop(&mut self) {
        if let Some(resources) = self.resources.as_ref() {
            warn!(
                display_id = %resources.display_id,
                "added cast display dropped without orderly media-driver shutdown"
            );
            warn!(
                display_id = %resources.display_id,
                "added cast display dropped without orderly kernel detach"
            );
        }
    }
}

fn set_status(
    status: &watch::Sender<DisplaySetupSnapshot>,
    stage: DisplaySetupStage,
    error_code: OperationErrorCode,
    error: Option<String>,
) {
    let error = error.map(|value| bounded_text(value, MAX_OPERATION_ERROR_BYTES));
    status.send_modify(|snapshot| {
        snapshot.stage = stage;
        snapshot.error_code = error_code;
        snapshot.error = error;
    });
}

fn bounded_text(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

#[derive(Debug, Error)]
pub enum DisplaySetupStartError {
    #[error("invalid local device-preparation offer: {0}")]
    InvalidOffer(String),
    #[error("generate cast-display identity: {0}")]
    Identity(#[source] io::Error),
}

#[derive(Debug, Error)]
pub enum DisplaySetupError {
    #[error("display setup was cancelled")]
    Cancelled,
    #[error("display-setup caller exited")]
    CallerExited,
    #[error("wait for display-setup caller exit: {0}")]
    CallerMonitor(#[source] io::Error),
    #[error("caller monitor task failed: {0}")]
    CallerTask(tokio::task::JoinError),
    #[error("pending display reservation was already consumed")]
    ReservationConsumed,
    #[error("reserve a CastKMS output for the selected Device: {0}")]
    Reserve(#[source] ReserveDisplaySlotError),
    #[error("selected Device changed during display setup: {0}")]
    Device(#[source] ResolveDeviceError),
    #[error("acquire kernel display session: {0}")]
    KernelSession(#[source] KernelSessionError),
    #[error("backend display session failed: {0}")]
    Backend(#[source] BackendSessionError),
    #[error("prepare selected Device identity and EDID: {0}")]
    Prepare(#[source] PrepareCastDeviceError),
    #[error("derive kernel access: {0}")]
    KernelAccess(#[source] io::Error),
    #[error("attach selected Device through monitor control: {0}")]
    KernelAttach(#[source] AttachError),
    #[error("start CastKMS display monitor: {0}")]
    Monitor(String),
}

impl DisplaySetupError {
    fn operation_error_code(&self) -> OperationErrorCode {
        match self {
            Self::Cancelled => OperationErrorCode::Cancelled,
            Self::CallerExited => OperationErrorCode::CallerExited,
            Self::Reserve(ReserveDisplaySlotError::Device(error)) | Self::Device(error) => {
                resolve_device_error_code(error)
            }
            Self::Reserve(ReserveDisplaySlotError::Output(
                OutputReservationError::CapacityExhausted,
            )) => OperationErrorCode::CapacityExhausted,
            Self::Reserve(ReserveDisplaySlotError::Output(
                OutputReservationError::DeviceAlreadyClaimed { .. },
            )) => OperationErrorCode::DeviceAlreadyAdded,
            Self::KernelSession(KernelSessionError::UnsupportedAudio) => {
                OperationErrorCode::InvalidRequest
            }
            Self::KernelSession(_) => OperationErrorCode::AuthorizationFailed,
            Self::Backend(error) => backend_session_error_code(error),
            Self::Prepare(error) => prepare_device_error_code(error),
            Self::KernelAttach(AttachError::Cancelled) => OperationErrorCode::Cancelled,
            Self::KernelAttach(AttachError::Rejected(_)) => OperationErrorCode::AttachmentFailed,
            Self::CallerMonitor(_)
            | Self::CallerTask(_)
            | Self::ReservationConsumed
            | Self::Reserve(_)
            | Self::KernelAttach(_)
            | Self::KernelAccess(_)
            | Self::Monitor(_) => OperationErrorCode::Internal,
        }
    }
}

fn backend_session_error_code(error: &BackendSessionError) -> OperationErrorCode {
    match error {
        BackendSessionError::BackendUnavailable
        | BackendSessionError::SupervisorStopped
        | BackendSessionError::MonitorStopped
        | BackendSessionError::MethodTimeout(_)
        | BackendSessionError::Protocol(_)
        | BackendSessionError::InvalidStatistics(_)
        | BackendSessionError::StaleStatisticsGeneration { .. }
        | BackendSessionError::InvalidControlCompletion(_)
        | BackendSessionError::ControlFailed(_)
        | BackendSessionError::ControlCompletionStreamClosed => {
            OperationErrorCode::BackendUnavailable
        }
        BackendSessionError::StaleConnectionGeneration { .. }
        | BackendSessionError::StaleDiscoveryGeneration { .. }
        | BackendSessionError::StalePreparationGeneration { .. } => {
            OperationErrorCode::DeviceChanged
        }
        BackendSessionError::DeviceUnavailable(_) => OperationErrorCode::DeviceUnavailable,
        BackendSessionError::UnexpectedObjectPath { .. }
        | BackendSessionError::InvalidCapabilities(_)
        | BackendSessionError::CapabilitiesOutsideOffer(_) => {
            OperationErrorCode::DevicePreparationFailed
        }
        BackendSessionError::InvalidRequest(_) => OperationErrorCode::Internal,
    }
}

fn prepare_device_error_code(error: &PrepareCastDeviceError) -> OperationErrorCode {
    match error {
        PrepareCastDeviceError::DeviceUnavailable(_) => OperationErrorCode::DeviceUnavailable,
        PrepareCastDeviceError::InvalidCapabilities(_)
        | PrepareCastDeviceError::UnsupportedIdentitySource { .. }
        | PrepareCastDeviceError::Pnp(_)
        | PrepareCastDeviceError::NoSupportedMode
        | PrepareCastDeviceError::MissingRequired640x480
        | PrepareCastDeviceError::Edid(_) => OperationErrorCode::DevicePreparationFailed,
        PrepareCastDeviceError::InvalidDevice(_) => OperationErrorCode::Internal,
    }
}

fn resolve_device_error_code(error: &ResolveDeviceError) -> OperationErrorCode {
    match error {
        ResolveDeviceError::InvalidSelection(_) => OperationErrorCode::InvalidRequest,
        ResolveDeviceError::NotFound { .. } => OperationErrorCode::DeviceNotFound,
        ResolveDeviceError::StaleSelection { .. } => OperationErrorCode::DeviceChanged,
        ResolveDeviceError::Unavailable { .. } => OperationErrorCode::DeviceUnavailable,
        ResolveDeviceError::BackendUnavailable { .. } => OperationErrorCode::BackendUnavailable,
        ResolveDeviceError::ManagerStopped => OperationErrorCode::Internal,
    }
}

#[derive(Debug, Error)]
pub enum DisplaySetupOperationError {
    #[error("display setup operation was already finished")]
    AlreadyFinished,
    #[error("display setup task failed: {0}")]
    Task(tokio::task::JoinError),
    #[error("display setup failed: {0}")]
    Setup(#[source] DisplaySetupError),
}

#[derive(Debug, Error)]
#[error("remove cast display failed (media={media:?}, detach={detach:?})")]
pub struct RemoveCastDisplayError {
    pub recovery: Option<String>,
    pub media: Option<String>,
    pub detach: Option<String>,
}

#[cfg(test)]
#[path = "display/kernel_session_tests.rs"]
mod kernel_session_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use pronk_core::identity::DEFAULT_SYNTHESIZER_PNP_ID;
    use pronk_core::output::{CastKmsOutput, CastKmsOutputId, OutputConnection};
    use pronk_dbus::DeviceAvailability;

    use super::*;
    use crate::manager::{
        test_reserved_display_slot, ManagerActor, OutputInventoryProvider,
        OutputInventoryProviderError,
    };

    #[derive(Debug)]
    struct CancellationKernelSessionProvider {
        entered: Arc<tokio::sync::Notify>,
        observed_cancellation: Arc<AtomicBool>,
    }

    #[derive(Debug)]
    struct UnreachableOutputProvider;

    impl OutputInventoryProvider for UnreachableOutputProvider {
        fn discover(&self) -> Result<Vec<CastKmsOutput>, OutputInventoryProviderError> {
            panic!("invalid Device selection reached DRM discovery")
        }
    }

    #[derive(Debug)]
    struct UnreachableKernelSessionProvider;

    #[async_trait::async_trait]
    impl KernelSessionProvider for UnreachableKernelSessionProvider {
        async fn acquire(
            &self,
            _output: &pronk_core::output::CastKmsOutput,
            _audio_enabled: bool,
            _cancellation: CancellationToken,
        ) -> Result<KernelSession, KernelSessionError> {
            panic!("invalid Device selection reached grant acquisition")
        }
    }

    #[async_trait::async_trait]
    impl KernelSessionProvider for CancellationKernelSessionProvider {
        async fn acquire(
            &self,
            _output: &pronk_core::output::CastKmsOutput,
            _audio_enabled: bool,
            cancellation: CancellationToken,
        ) -> Result<KernelSession, KernelSessionError> {
            self.entered.notify_one();
            cancellation.cancelled().await;
            self.observed_cancellation.store(true, Ordering::SeqCst);
            Err(KernelSessionError::Cancelled)
        }
    }

    #[tokio::test]
    async fn pending_operation_returns_before_device_validation() {
        let resolver = Arc::new(
            PnpIdResolver::from_database("GGL\tGoogle Inc.\n", &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .unwrap(),
        );
        let provider: Arc<dyn KernelSessionProvider> = Arc::new(UnreachableKernelSessionProvider);
        let manager = ManagerActor::spawn_with_providers(
            Vec::new(),
            Arc::new(UnreachableOutputProvider),
            Arc::clone(&provider),
            Arc::clone(&resolver),
        )
        .unwrap();
        let caller = DisplaySetupCaller {
            exit: Box::pin(std::future::pending()),
        };
        let operation = DisplaySetupOperation::spawn_with_caller(
            DisplayReservation::Pending {
                manager: manager.handle(),
                pending: PendingDisplaySelection {
                    selection: DeviceSelection {
                        backend_id: "mock".into(),
                        device_id: "missing".into(),
                        connection_generation: 1,
                        discovery_generation: 1,
                        device_revision: 1,
                    },
                    preferred_output: None,
                },
            },
            caller,
            DisplaySetupDependencies::new(
                provider,
                resolver,
                MediaRuntime::for_user(Uid::effective().as_raw()),
                crate::preparation::initial_preparation_offer(
                    false,
                    CaptureSource::Renderer.initial_raw_layouts(),
                ),
                false,
            ),
            DisplaySetupStage::Validating,
        )
        .unwrap();
        let status = operation.subscribe();
        assert_eq!(operation.snapshot().stage, DisplaySetupStage::Validating);
        assert!(matches!(
            operation.finish().await,
            Err(DisplaySetupOperationError::Setup(
                DisplaySetupError::Reserve(ReserveDisplaySlotError::Device(
                    ResolveDeviceError::NotFound { .. }
                ))
            ))
        ));
        assert_eq!(status.borrow().stage, DisplaySetupStage::Failed);
        assert_eq!(
            status.borrow().error_code,
            OperationErrorCode::DeviceNotFound
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_during_authorization_releases_the_pending_slot() {
        let device = DeviceInfo {
            backend_id: "mock".into(),
            device_id: "living-room".into(),
            display_name: "Living Room TV".into(),
            availability: DeviceAvailability::Available,
            connection_generation: 1,
            discovery_generation: 2,
            device_revision: 3,
            metadata: Vec::new(),
        };
        let output = CastKmsOutput {
            id: CastKmsOutputId {
                device_path: PathBuf::from("/sys/devices/virtual/castkms"),
                output_index: 0,
            },
            node_path: PathBuf::from("/dev/dri/card9"),
            device_major: 226,
            device_minor: 9,
            crtc_id: 20,
            connector_id: 40,
            connector_name: "Virtual-1".into(),
            connection: OutputConnection::Disconnected,
        };
        let (slot, mut releases) = test_reserved_display_slot(device, output);
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed_cancellation = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn KernelSessionProvider> =
            Arc::new(CancellationKernelSessionProvider {
                entered: Arc::clone(&entered),
                observed_cancellation: Arc::clone(&observed_cancellation),
            });
        let resolver = Arc::new(
            PnpIdResolver::from_database("GGL\tGoogle Inc.\n", &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .unwrap(),
        );
        let caller = DisplaySetupCaller {
            exit: Box::pin(std::future::pending()),
        };
        let operation = DisplaySetupOperation::spawn_with_caller(
            DisplayReservation::Ready(Box::new(slot)),
            caller,
            DisplaySetupDependencies::new(
                provider,
                resolver,
                MediaRuntime::for_user(Uid::effective().as_raw()),
                crate::preparation::initial_preparation_offer(
                    false,
                    CaptureSource::Renderer.initial_raw_layouts(),
                ),
                false,
            ),
            DisplaySetupStage::Authorizing,
        )
        .unwrap();
        let status = operation.subscribe();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert_eq!(operation.snapshot().stage, DisplaySetupStage::Authorizing);
        operation.cancel();
        assert!(matches!(
            operation.finish().await,
            Err(DisplaySetupOperationError::Setup(
                DisplaySetupError::Cancelled
            ))
        ));
        assert!(observed_cancellation.load(Ordering::SeqCst));
        assert_eq!(status.borrow().stage, DisplaySetupStage::Cancelled);
        assert_eq!(status.borrow().error_code, OperationErrorCode::Cancelled);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), releases.recv())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn generated_display_ids_are_canonical_random_uuids() {
        let first = CastDisplayId::generate().unwrap();
        let second = CastDisplayId::generate().unwrap();
        assert_ne!(first, second);
        let text = first.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(&text[14..15], "4");
        assert!(matches!(&text[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(
            text.chars().filter(|character| *character == '-').count(),
            4
        );
        assert_eq!(text.parse::<CastDisplayId>().unwrap(), first);
        assert_eq!(first.object_segment().len(), 32);
        assert!("00000000-0000-0000-0000-000000000000"
            .parse::<CastDisplayId>()
            .is_err());
    }

    #[test]
    fn operation_errors_are_utf8_bounded() {
        let text = format!("{}é", "x".repeat(MAX_OPERATION_ERROR_BYTES));
        let bounded = bounded_text(text, MAX_OPERATION_ERROR_BYTES);
        assert_eq!(bounded.len(), MAX_OPERATION_ERROR_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
