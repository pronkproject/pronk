//! Cancellable transition from a pending output reservation to one attached
//! cast display.

use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use nix::{libc, unistd::Uid};
use pronk_backend_host::{BackendSessionError, BackendSessionHandle};
use pronk_backend_protocol::{PreparationRequest, StopReason, Validate};
use pronk_core::castkms::{CastKmsClient, CastKmsError};
use pronk_core::grant::{GrantAcquisitionError, GrantProvider};
use pronk_core::grant::{GrantProfile, GrantTarget};
use pronk_core::identity::PnpIdResolver;
use pronk_core::output::CastKmsOutputId;
use pronk_core::session::PinnedCallerProcess;
use pronk_dbus::{DeviceInfo, DeviceSelection, OperationErrorCode};
use pronk_pipewire::{ClassifiedSocketPaths, ClassifiedSocketRemoteProvider};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::castkms_actor::{CastKmsActorConfig, CastKmsKernelActor};
use crate::device_recovery::{
    DeviceSessionFactoryError, DeviceSessionFactoryPort, PreparedDeviceSession,
};
use crate::device_session::{BackendDeviceSession, BackendDeviceSessionEvents};
use crate::device_session_port::DeviceSessionEventPort;
use crate::display_state::{DisplayGrantState, DisplayRuntimeState};
use crate::kernel_display_port::KernelDisplayPort;
use crate::manager::{
    DeviceSessionResolver, ManagerHandle, ReserveDisplaySlotError, ReservedCastDisplaySlot,
    ResolveDeviceError,
};
use crate::media_driver::ProductionMediaSessionDriver;
use crate::media_remote::ClassifiedDeviceMediaRemotePort;
use crate::media_session::MediaSessionDriver;
use crate::preparation::{PrepareCastDeviceError, PreparedCastDevice};
use crate::replaceable_device_session::{
    replaceable_device_session, DeviceSessionReplacementHandle,
};
use crate::slot::OutputReservationError;

const INITIAL_SESSION_GENERATION: u64 = 1;
const MAX_OPERATION_ERROR_BYTES: usize = 512;

/// PipeWire runtime owned by the account running the media services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRuntime {
    directory: PathBuf,
    server_uid: u32,
}

impl MediaRuntime {
    pub fn new(directory: PathBuf, server_uid: u32) -> Self {
        Self {
            directory,
            server_uid,
        }
    }

    pub fn for_user(uid: u32) -> Self {
        Self::new(PathBuf::from(format!("/run/user/{uid}/pronk/media")), uid)
    }
}

pub(crate) struct DisplaySetupDependencies {
    grant_provider: Arc<dyn GrantProvider>,
    pnp_resolver: Arc<PnpIdResolver>,
    media_runtime: MediaRuntime,
    offer: PreparationRequest,
    audio_enabled: bool,
}

impl DisplaySetupDependencies {
    pub(crate) fn new(
        grant_provider: Arc<dyn GrantProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
        media_runtime: MediaRuntime,
        offer: PreparationRequest,
        audio_enabled: bool,
    ) -> Self {
        Self {
            grant_provider,
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
        grant_provider: Arc<dyn GrantProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
        media_runtime: MediaRuntime,
        offer: PreparationRequest,
        audio_enabled: bool,
    ) -> Result<Self, DisplaySetupStartError> {
        Self::spawn_with_caller(
            DisplayReservation::Ready(Box::new(slot)),
            DisplaySetupCaller::from(caller),
            DisplaySetupDependencies::new(
                grant_provider,
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
                    grant_provider: dependencies.grant_provider,
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
    display_id: CastDisplayId,
    state_revision: u64,
    device: DeviceInfo,
    prepared: Option<PreparedCastDevice>,
    slot: Option<ReservedCastDisplaySlot>,
    media_driver: Option<Box<dyn MediaSessionDriver>>,
    recovery_factory: Option<Box<dyn DeviceSessionFactoryPort>>,
    session_replacement: Option<DeviceSessionReplacementHandle>,
    initial_session_generation: NonZeroU64,
    session_events: Option<Box<dyn DeviceSessionEventPort>>,
    kernel: Option<Box<dyn KernelDisplayPort>>,
}

/// Bounded read-only projection of a manager-owned added display.
///
/// This is an internal control-plane snapshot. The public D-Bus adapter uses a
/// narrower protocol type and never exposes DRM node paths or grant IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedCastDisplaySnapshot {
    pub display_id: CastDisplayId,
    pub state_revision: u64,
    pub device: DeviceInfo,
    pub prepared: PreparedCastDevice,
    pub output: pronk_core::output::CastKmsOutput,
    pub grant_id: u32,
    pub grant_state: DisplayGrantState,
    pub runtime: DisplayRuntimeState,
}

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
    pub fn display_id(&self) -> CastDisplayId {
        self.display_id
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub(crate) fn update_device(&mut self, device: DeviceInfo) -> bool {
        debug_assert_eq!(self.device.backend_id, device.backend_id);
        debug_assert_eq!(self.device.device_id, device.device_id);
        if self.device == device {
            return false;
        }
        self.state_revision = self
            .state_revision
            .saturating_add(1)
            .max(device.device_revision);
        self.device = device;
        true
    }

    pub(crate) fn into_resources(mut self) -> AddedCastDisplayResources {
        AddedCastDisplayResources {
            display_id: self.display_id,
            state_revision: self.state_revision,
            device: self.device.clone(),
            prepared: self
                .prepared
                .take()
                .expect("added display owns its prepared Device"),
            slot: self
                .slot
                .take()
                .expect("added display owns its output slot"),
            media_driver: self
                .media_driver
                .take()
                .expect("added display owns its media driver"),
            recovery_factory: self
                .recovery_factory
                .take()
                .expect("added display owns its Device-session recovery factory"),
            session_replacement: self
                .session_replacement
                .take()
                .expect("added display owns its Device-session replacement handle"),
            initial_session_generation: self.initial_session_generation,
            session_events: self
                .session_events
                .take()
                .expect("added display owns its Device-session event source"),
            kernel: self
                .kernel
                .take()
                .expect("added display owns its kernel display"),
        }
    }
}

impl Drop for AddedCastDisplay {
    fn drop(&mut self) {
        if self.media_driver.is_some() {
            warn!(
                display_id = %self.display_id,
                "added cast display dropped without orderly media-driver shutdown"
            );
        }
        if self.kernel.is_some() {
            warn!(
                display_id = %self.display_id,
                "added cast display dropped without orderly kernel detach"
            );
        }
    }
}

struct DisplaySetupContext {
    display_id: CastDisplayId,
    grant_provider: Arc<dyn GrantProvider>,
    pnp_resolver: Arc<PnpIdResolver>,
    media_runtime: MediaRuntime,
    offer: PreparationRequest,
    audio_enabled: bool,
    cancellation: CancellationToken,
    status: watch::Sender<DisplaySetupSnapshot>,
}

enum DisplayReservation {
    Ready(Box<ReservedCastDisplaySlot>),
    Pending {
        manager: ManagerHandle,
        pending: PendingDisplaySelection,
    },
}

struct DisplaySetupCaller {
    exit: Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>>,
}

impl From<PinnedCallerProcess> for DisplaySetupCaller {
    fn from(caller: PinnedCallerProcess) -> Self {
        let exit = Box::pin(async move { caller.wait_for_exit().await });
        Self { exit }
    }
}

async fn run_display_setup(
    reservation: DisplayReservation,
    caller: DisplaySetupCaller,
    context: DisplaySetupContext,
) -> Result<AddedCastDisplay, DisplaySetupError> {
    let caller_gone = CancellationToken::new();
    let linked_cancellation = context.cancellation.child_token();
    let caller_gone_for_task = caller_gone.clone();
    let linked_for_task = linked_cancellation.clone();
    let mut caller_task = tokio::spawn(async move {
        let result = caller.exit.await;
        caller_gone_for_task.cancel();
        linked_for_task.cancel();
        result
    });

    let context = DisplaySetupContext {
        cancellation: linked_cancellation,
        ..context
    };
    let slot = match reservation {
        DisplayReservation::Ready(slot) => Ok(*slot),
        DisplayReservation::Pending { manager, pending } => {
            let reservation =
                manager.reserve_display_slot(pending.selection, pending.preferred_output);
            tokio::pin!(reservation);
            let result = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    Err(DisplaySetupError::Cancelled)
                }
                result = &mut reservation => {
                    result.map_err(DisplaySetupError::Reserve)
                }
            };
            if result.is_ok() {
                set_status(
                    &context.status,
                    DisplaySetupStage::Authorizing,
                    OperationErrorCode::None,
                    None,
                );
            }
            result
        }
    };
    let result = match slot {
        Ok(slot) => run_display_setup_inner(slot, context).await,
        Err(error) => Err(error),
    };
    if matches!(result, Err(DisplaySetupError::Cancelled)) && caller_gone.is_cancelled() {
        return match (&mut caller_task).await {
            Ok(Ok(())) => Err(DisplaySetupError::CallerExited),
            Ok(Err(error)) => Err(DisplaySetupError::CallerMonitor(error)),
            Err(error) => Err(DisplaySetupError::CallerTask(error)),
        };
    }
    caller_task.abort();
    let _ = caller_task.await;
    result
}

async fn run_display_setup_inner(
    mut slot: ReservedCastDisplaySlot,
    context: DisplaySetupContext,
) -> Result<AddedCastDisplay, DisplaySetupError> {
    if context.cancellation.is_cancelled() {
        return Err(DisplaySetupError::Cancelled);
    }
    let device = slot.device().clone();
    let output = slot.output().clone();
    let grant_profile = if context.audio_enabled {
        GrantProfile::DisplayCecAudioV1
    } else {
        GrantProfile::DisplayCecV1
    };
    let lease = context
        .grant_provider
        .acquire(
            GrantTarget {
                device_major: output.device_major,
                device_minor: output.device_minor,
                connector_id: output.connector_id,
                profile: grant_profile,
            },
            context.cancellation.clone(),
        )
        .await
        .map_err(|error| match error {
            GrantAcquisitionError::Cancelled => DisplaySetupError::Cancelled,
            error => DisplaySetupError::Grant(error),
        })?;
    if context.cancellation.is_cancelled() {
        return Err(DisplaySetupError::Cancelled);
    }
    {
        let revalidation = slot.revalidate_device();
        tokio::pin!(revalidation);
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => return Err(DisplaySetupError::Cancelled),
            result = &mut revalidation => result.map_err(DisplaySetupError::Device)?,
        }
    }
    let client = CastKmsClient::new(lease).map_err(DisplaySetupError::GrantClient)?;

    set_status(
        &context.status,
        DisplaySetupStage::PreparingDevice,
        OperationErrorCode::None,
        None,
    );
    let selection = slot
        .take_selection()
        .ok_or(DisplaySetupError::ReservationConsumed)?;
    let mut create_session = Box::pin(selection.create_session(
        context.display_id.to_string(),
        INITIAL_SESSION_GENERATION,
        context.offer.requested_features,
    ));
    let backend_session = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            if let Ok(session) = create_session.await {
                stop_partial_backend(session).await;
            }
            return Err(DisplaySetupError::Cancelled);
        }
        result = &mut create_session => result.map_err(DisplaySetupError::Backend)?,
    };

    let mut prepare = Box::pin(backend_session.prepare(context.offer.clone()));
    let capabilities = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            drop(prepare);
            stop_partial_backend(backend_session).await;
            return Err(DisplaySetupError::Cancelled);
        }
        result = &mut prepare => match result {
            Ok(capabilities) => capabilities,
            Err(error) => {
                drop(prepare);
                stop_partial_backend(backend_session).await;
                return Err(DisplaySetupError::Backend(error));
            }
        },
    };
    drop(prepare);

    let prepared = match PreparedCastDevice::from_capabilities(
        device.clone(),
        capabilities,
        &context.pnp_resolver,
        context.audio_enabled,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            stop_partial_backend(backend_session).await;
            return Err(DisplaySetupError::Prepare(error));
        }
    };
    if context.cancellation.is_cancelled() {
        stop_partial_backend(backend_session).await;
        return Err(DisplaySetupError::Cancelled);
    }
    let revalidation = {
        let revalidation = slot.revalidate_device();
        tokio::pin!(revalidation);
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => Err(DisplaySetupError::Cancelled),
            result = &mut revalidation => result.map_err(DisplaySetupError::Device),
        }
    };
    if let Err(error) = revalidation {
        stop_partial_backend(backend_session).await;
        return Err(error);
    }
    let session_resolver = match slot.device_session_resolver() {
        Ok(resolver) => resolver,
        Err(error) => {
            stop_partial_backend(backend_session).await;
            return Err(DisplaySetupError::Device(error));
        }
    };
    let mut start_event_monitor = Box::pin(BackendDeviceSessionEvents::start(&backend_session));
    let session_events = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            // The monitor startup future owns an abort-on-drop task. No
            // protocol authority is created here, so cancellation should not
            // wait out its independent five-second startup deadline.
            drop(start_event_monitor);
            stop_partial_backend(backend_session).await;
            return Err(DisplaySetupError::Cancelled);
        }
        result = &mut start_event_monitor => match result {
            Ok(events) => events,
            Err(error) => {
                drop(start_event_monitor);
                stop_partial_backend(backend_session).await;
                return Err(DisplaySetupError::Backend(error));
            }
        },
    };
    drop(start_event_monitor);

    set_status(
        &context.status,
        DisplaySetupStage::Attaching,
        OperationErrorCode::None,
        None,
    );
    let edid = prepared.generated_edid().edid().clone();
    let assigned_display_name = device.display_name.clone();
    let mut attach_task = tokio::task::spawn_blocking(move || {
        let result = client.attach_monitor(&edid, &assigned_display_name);
        (client, result)
    });
    let (client, attach_result) = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            let joined = (&mut attach_task).await;
            stop_partial_backend(backend_session).await;
            match joined {
                Ok((client, Ok(()))) => {
                    if let Err(error) = detach_client(client).await {
                        warn!(%error, "cancelled setup could not explicitly detach its monitor");
                    }
                }
                Ok((_, Err(_))) | Err(_) => {}
            }
            return Err(DisplaySetupError::Cancelled);
        }
        joined = &mut attach_task => joined.map_err(DisplaySetupError::AttachTask)?,
    };
    if let Err(error) = attach_result {
        stop_partial_backend(backend_session).await;
        return Err(DisplaySetupError::Attach(error));
    }
    if context.cancellation.is_cancelled() {
        stop_partial_backend(backend_session).await;
        if let Err(error) = detach_client(client).await {
            warn!(%error, "cancelled setup could not explicitly detach its monitor");
        }
        return Err(DisplaySetupError::Cancelled);
    }

    let pipewire_paths =
        match ClassifiedSocketPaths::in_runtime_dir(context.media_runtime.directory.clone()) {
            Ok(paths) => paths,
            Err(error) => {
                stop_partial_backend(backend_session).await;
                if let Err(detach_error) = detach_client(client).await {
                    warn!(%detach_error, "failed to detach after PipeWire path validation failed");
                }
                return Err(DisplaySetupError::Monitor(format!(
                    "construct classified PipeWire paths: {error}"
                )));
            }
        };
    let video_profile_id = prepared.capabilities().video_profiles[0].profile_id.clone();
    let audio_profile_id = (context.audio_enabled
        && !prepared.capabilities().audio_profiles.is_empty())
    .then(|| prepared.capabilities().audio_profiles[0].profile_id.clone());
    let remote_provider = ClassifiedSocketRemoteProvider::new_for_server_uid(
        pipewire_paths,
        Uid::from_raw(context.media_runtime.server_uid),
    );
    let session_id = context.display_id.to_string();
    let initial_session_generation =
        NonZeroU64::new(INITIAL_SESSION_GENERATION).expect("initial session generation is nonzero");
    let (device_session, device_control, session_replacement) = replaceable_device_session(
        initial_session_generation,
        Box::new(BackendDeviceSession::new(backend_session)),
    );
    let actor_config = CastKmsActorConfig {
        producer_remotes: remote_provider.clone(),
        session_id: session_id.clone(),
        device_instance: format!("cast-display-{}", context.display_id.object_segment()),
        node_description: device.display_name.clone(),
        output_index: output.id.output_index,
        video_profile_id,
        audio_profile_id,
        video_bitrate: NonZeroU64::new(8_000_000).expect("fixed bitrate is nonzero"),
        device_control: prepared
            .control_enabled()
            .then(|| Arc::clone(&device_control)),
    };
    let (kernel, capture) = match CastKmsKernelActor::spawn(client, actor_config) {
        Ok(parts) => parts,
        Err(error) => {
            if let Err(cleanup_error) = device_session
                .stop(crate::device_session_port::DeviceSessionStopReason::DaemonShutdown)
                .await
            {
                warn!(%cleanup_error, "failed to stop Device session after CastKMS actor startup failed");
            }
            return Err(DisplaySetupError::Monitor(error.to_string()));
        }
    };
    let kernel = Box::new(kernel) as Box<dyn KernelDisplayPort>;
    let remote_port = ClassifiedDeviceMediaRemotePort::new(
        remote_provider,
        session_id,
        device.backend_id.clone(),
    );
    let media_driver =
        ProductionMediaSessionDriver::new(Box::new(capture), Box::new(remote_port), device_session);
    let recovery_factory = BackendPreparedDeviceSessionFactory {
        resolver: session_resolver,
        session_id: context.display_id.to_string(),
        offer: context.offer,
        pnp_resolver: Arc::clone(&context.pnp_resolver),
        audio_enabled: context.audio_enabled,
    };
    let state_revision = device.device_revision;
    Ok(AddedCastDisplay {
        display_id: context.display_id,
        state_revision,
        device,
        prepared: Some(prepared),
        slot: Some(slot),
        media_driver: Some(Box::new(media_driver)),
        recovery_factory: Some(Box::new(recovery_factory)),
        session_replacement: Some(session_replacement),
        initial_session_generation,
        session_events: Some(Box::new(session_events)),
        kernel: Some(kernel),
    })
}

#[derive(Debug)]
struct BackendPreparedDeviceSessionFactory {
    resolver: DeviceSessionResolver,
    session_id: String,
    offer: PreparationRequest,
    pnp_resolver: Arc<PnpIdResolver>,
    audio_enabled: bool,
}

#[async_trait::async_trait]
impl DeviceSessionFactoryPort for BackendPreparedDeviceSessionFactory {
    async fn create_prepared_session(
        &mut self,
        device: DeviceInfo,
        session_generation: NonZeroU64,
        cancellation: CancellationToken,
    ) -> Result<PreparedDeviceSession, DeviceSessionFactoryError> {
        let selection = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(DeviceSessionFactoryError::Cancelled),
            result = self.resolver.resolve(&device) => result.map_err(|error| {
                DeviceSessionFactoryError::failed(format!("resolve current Device: {error}"))
            })?,
        };
        let mut create_session = Box::pin(selection.create_session(
            self.session_id.clone(),
            session_generation.get(),
            self.offer.requested_features,
        ));
        let backend_session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                if let Ok(session) = create_session.await {
                    stop_partial_backend(session).await;
                }
                return Err(DeviceSessionFactoryError::Cancelled);
            }
            result = &mut create_session => result.map_err(|error| {
                DeviceSessionFactoryError::failed(format!("create replacement Device session: {error}"))
            })?,
        };

        let mut prepare = Box::pin(backend_session.prepare(self.offer.clone()));
        let capabilities = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                drop(prepare);
                stop_partial_backend(backend_session).await;
                return Err(DeviceSessionFactoryError::Cancelled);
            }
            result = &mut prepare => match result {
                Ok(capabilities) => capabilities,
                Err(error) => {
                    drop(prepare);
                    stop_partial_backend(backend_session).await;
                    return Err(DeviceSessionFactoryError::failed(format!(
                        "prepare replacement Device session: {error}"
                    )));
                }
            },
        };
        drop(prepare);

        let prepared = match PreparedCastDevice::from_capabilities(
            device,
            capabilities,
            &self.pnp_resolver,
            self.audio_enabled,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                stop_partial_backend(backend_session).await;
                return Err(DeviceSessionFactoryError::failed(format!(
                    "validate replacement Device preparation: {error}"
                )));
            }
        };
        if cancellation.is_cancelled() {
            stop_partial_backend(backend_session).await;
            return Err(DeviceSessionFactoryError::Cancelled);
        }
        let mut start_event_monitor = Box::pin(BackendDeviceSessionEvents::start(&backend_session));
        let events = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                drop(start_event_monitor);
                stop_partial_backend(backend_session).await;
                return Err(DeviceSessionFactoryError::Cancelled);
            }
            result = &mut start_event_monitor => match result {
                Ok(events) => events,
                Err(error) => {
                    drop(start_event_monitor);
                    stop_partial_backend(backend_session).await;
                    return Err(DeviceSessionFactoryError::failed(format!(
                        "monitor replacement Device session: {error}"
                    )));
                }
            },
        };
        drop(start_event_monitor);
        Ok(PreparedDeviceSession {
            prepared,
            session: Box::new(BackendDeviceSession::new(backend_session)),
            events: Box::new(events),
        })
    }
}

async fn stop_partial_backend(session: BackendSessionHandle) {
    if let Err(error) = session.stop(StopReason::UserRequest).await {
        warn!(%error, "failed to stop a partial backend display session");
    }
}

async fn detach_client(client: CastKmsClient) -> Result<(), String> {
    tokio::task::spawn_blocking(move || client.detach_monitor().map_err(|error| error.to_string()))
        .await
        .map_err(|error| format!("detach task failed: {error}"))?
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
    #[error("acquire connector grant: {0}")]
    Grant(#[source] GrantAcquisitionError),
    #[error("open verified CastKMS grant client: {0}")]
    GrantClient(#[source] CastKmsError),
    #[error("backend display session failed: {0}")]
    Backend(#[source] BackendSessionError),
    #[error("prepare selected Device identity and EDID: {0}")]
    Prepare(#[source] PrepareCastDeviceError),
    #[error("attach selected Device EDID: {0}")]
    Attach(#[source] CastKmsError),
    #[error("CastKMS attach task failed: {0}")]
    AttachTask(tokio::task::JoinError),
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
            Self::Grant(_) | Self::GrantClient(_) => OperationErrorCode::AuthorizationFailed,
            Self::Backend(error) => backend_session_error_code(error),
            Self::Prepare(error) => prepare_device_error_code(error),
            Self::Attach(_) => OperationErrorCode::AttachmentFailed,
            Self::CallerMonitor(_)
            | Self::CallerTask(_)
            | Self::ReservationConsumed
            | Self::Reserve(_)
            | Self::AttachTask(_)
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
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use pronk_core::grant::GrantLease;
    use pronk_core::identity::DEFAULT_SYNTHESIZER_PNP_ID;
    use pronk_core::output::{CastKmsOutput, CastKmsOutputId, OutputConnection};
    use pronk_dbus::DeviceAvailability;

    use super::*;
    use crate::manager::{
        test_reserved_display_slot, ManagerActor, OutputInventoryProvider,
        OutputInventoryProviderError,
    };

    #[derive(Debug)]
    struct CancellationGrantProvider {
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
    struct UnreachableGrantProvider;

    #[async_trait::async_trait]
    impl GrantProvider for UnreachableGrantProvider {
        async fn acquire(
            &self,
            _target: GrantTarget,
            _cancellation: CancellationToken,
        ) -> Result<GrantLease, GrantAcquisitionError> {
            panic!("invalid Device selection reached grant acquisition")
        }
    }

    #[async_trait::async_trait]
    impl GrantProvider for CancellationGrantProvider {
        async fn acquire(
            &self,
            _target: GrantTarget,
            cancellation: CancellationToken,
        ) -> Result<GrantLease, GrantAcquisitionError> {
            self.entered.notify_one();
            cancellation.cancelled().await;
            self.observed_cancellation.store(true, Ordering::SeqCst);
            Err(GrantAcquisitionError::Cancelled)
        }
    }

    #[tokio::test]
    async fn pending_operation_returns_before_device_validation() {
        let resolver = Arc::new(
            PnpIdResolver::from_database("GGL\tGoogle Inc.\n", &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .unwrap(),
        );
        let provider: Arc<dyn GrantProvider> = Arc::new(UnreachableGrantProvider);
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
                crate::preparation::initial_preparation_offer(false),
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
            connector_id: 40,
            connector_name: "Virtual-1".into(),
            connection: OutputConnection::Disconnected,
        };
        let (slot, mut releases) = test_reserved_display_slot(device, output);
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed_cancellation = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(CancellationGrantProvider {
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
                crate::preparation::initial_preparation_offer(false),
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
