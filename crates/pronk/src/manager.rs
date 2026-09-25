use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use nix::unistd::Uid;
use pronk_backend_host::{
    BackendEndpoint, BackendHandle, BackendReconnectPolicy, BackendRegistrationValidator,
    BackendSessionError, BackendSessionHandle, BackendSessionRequest, BackendShutdownReport,
    BackendSupervisor, BackendSupervisorError, MAX_INSTALLED_BACKENDS,
};
use pronk_backend_protocol::SessionOptions;
use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID, SYSTEM_PNP_IDS_PATH};
use pronk_core::output::{
    discover_castkms_outputs, CastKmsOutput, CastKmsOutputId, OutputDiscoveryError,
};
use pronk_core::session::PinnedCallerProcess;
use pronk_dbus::{DeviceAvailability, DeviceInfo, DeviceSelection, DeviceSnapshot};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout_at, Instant};
use tracing::{debug, warn};

use crate::cast_display_slot::CastDisplaySlotEvent;
use crate::device_session_port::DeviceSessionStopReason;
use crate::display::{
    AddedCastDisplaySnapshot, CastDisplayId, DisplaySetupDependencies, DisplaySetupHandle,
    DisplaySetupOperation, DisplaySetupStartError, MediaRuntime, PendingDisplaySelection,
};
use crate::kernel_session_provider::KernelSessionProvider;
use crate::preparation::initial_preparation_offer;
use crate::slot::{
    OutputReservation, OutputReservationError, OutputReservationRelease, OutputSlotPool,
};

mod backend_worker;
mod display_lifecycle;
mod inventory;
use backend_worker::{shutdown_workers, BackendWorker, BackendWorkerMessage};
use display_lifecycle::{
    handle_removal_join, handle_setup_join, start_managed_display_setup, ManagedDisplayPhase,
    ManagedDisplayRecord, RemovalCompletion, SetupCompletion,
};
use inventory::{
    configured_device_update, AggregateError, AggregateInventory, ApplySupervisorOutcome,
};

const MANAGER_COMMAND_QUEUE: usize = 32;
const MANAGER_EVENT_QUEUE: usize = 256;
const BACKEND_EVENT_QUEUE: usize = 256;
const MANAGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

pub struct BackendConfig {
    pub endpoint: BackendEndpoint,
    pub initial_connection_generation: u64,
    pub registration_validator: Arc<dyn BackendRegistrationValidator>,
    pub reconnect_policy: BackendReconnectPolicy,
}

pub trait OutputInventoryProvider: std::fmt::Debug + Send + Sync + 'static {
    fn discover(&self) -> Result<Vec<CastKmsOutput>, OutputInventoryProviderError>;
}

#[derive(Debug, Default)]
pub struct SystemOutputInventoryProvider;

impl OutputInventoryProvider for SystemOutputInventoryProvider {
    fn discover(&self) -> Result<Vec<CastKmsOutput>, OutputInventoryProviderError> {
        discover_castkms_outputs().map_err(OutputInventoryProviderError::from)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct OutputInventoryProviderError(String);

impl OutputInventoryProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<OutputDiscoveryError> for OutputInventoryProviderError {
    fn from(error: OutputDiscoveryError) -> Self {
        Self(error.to_string())
    }
}

impl BackendConfig {
    pub fn new(
        endpoint: BackendEndpoint,
        initial_connection_generation: u64,
        registration_validator: Arc<dyn BackendRegistrationValidator>,
        reconnect_policy: BackendReconnectPolicy,
    ) -> Self {
        Self {
            endpoint,
            initial_connection_generation,
            registration_validator,
            reconnect_policy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryEvent {
    DeviceAdded {
        inventory_revision: u64,
        device: DeviceInfo,
    },
    DeviceChanged {
        inventory_revision: u64,
        device: DeviceInfo,
    },
    DeviceRemoved {
        inventory_revision: u64,
        backend_id: String,
        device_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    DisplayAdded(Box<AddedCastDisplaySnapshot>),
    DisplayStateChanged(Box<AddedCastDisplaySnapshot>),
    DisplayRemoved { display_id: CastDisplayId },
}

struct ManagerEventSinks {
    inventory: mpsc::Sender<InventoryEvent>,
    lifecycle: mpsc::UnboundedSender<LifecycleEvent>,
}

#[derive(Debug, Clone)]
pub struct ManagerHandle {
    commands: mpsc::Sender<ManagerCommand>,
    output_provider: Arc<dyn OutputInventoryProvider>,
    kernel_session_provider: Arc<dyn KernelSessionProvider>,
    pnp_resolver: Arc<PnpIdResolver>,
    media_runtime: MediaRuntime,
}

impl ManagerHandle {
    pub async fn list_devices(&self) -> Result<DeviceSnapshot, ManagerRequestError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::ListDevices(response_tx))
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)
    }

    /// Resolve one exact inventory record into a one-shot backend selection.
    ///
    /// This is an internal setup primitive rather than a public D-Bus method.
    /// The future AddDisplay operation will call it from its manager-owned
    /// state machine after reserving a CastKMS slot.
    pub async fn resolve_device(
        &self,
        selection: DeviceSelection,
    ) -> Result<ResolvedDeviceSelection, ResolveDeviceError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::ResolveDevice {
                selection,
                response: response_tx,
            })
            .await
            .map_err(|_| ResolveDeviceError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ResolveDeviceError::ManagerStopped)?
    }

    /// Atomically revalidate one selected Device and reserve one discovered
    /// CastKMS output inside the manager task.
    ///
    /// This remains an internal lifecycle primitive; the public D-Bus API does
    /// not advertise AddDisplay until its operation-object contract is
    /// registered and covered by API tests.
    pub async fn reserve_display_slot(
        &self,
        selection: DeviceSelection,
        preferred_output: Option<CastKmsOutputId>,
    ) -> Result<ReservedCastDisplaySlot, ReserveDisplaySlotError> {
        // Reject a malformed/stale/unavailable selection before touching DRM.
        // The manager task repeats this check after discovery to close the
        // generation race.
        self.resolve_device(selection.clone())
            .await
            .map_err(ReserveDisplaySlotError::Device)?;
        let provider = Arc::clone(&self.output_provider);
        let outputs = tokio::task::spawn_blocking(move || provider.discover())
            .await
            .map_err(|error| ReserveDisplaySlotError::DiscoveryTask(error.to_string()))??;
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::ReserveDisplaySlot {
                selection,
                outputs,
                preferred_output,
                response: response_tx,
            })
            .await
            .map_err(|_| ReserveDisplaySlotError::ManagerStopped)?;
        let mut slot = response_rx
            .await
            .map_err(|_| ReserveDisplaySlotError::ManagerStopped)??;
        slot.manager_commands = Some(self.commands.clone());
        Ok(slot)
    }

    pub async fn list_displays(
        &self,
    ) -> Result<Vec<AddedCastDisplaySnapshot>, ManagerRequestError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::ListDisplays(response_tx))
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)
    }

    pub async fn display(
        &self,
        display_id: CastDisplayId,
    ) -> Result<Option<AddedCastDisplaySnapshot>, ManagerRequestError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::GetDisplay {
                display_id,
                response: response_tx,
            })
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)
    }

    /// Begin one manager-owned setup operation for an exact Device selection.
    ///
    /// The caller identity must already have been obtained from the selected
    /// bus broker and pidfd-pinned. No grant policy or DRM target is accepted
    /// from the public client. The returned handle observes and explicitly
    /// cancels the operation; dropping it does not cancel manager-owned work.
    pub async fn start_display_setup(
        &self,
        selection: DeviceSelection,
        preferred_output: Option<CastKmsOutputId>,
        caller: PinnedCallerProcess,
        audio_enabled: bool,
    ) -> Result<DisplaySetupHandle, StartDisplaySetupError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::StartDisplaySetup {
                selection,
                preferred_output,
                caller,
                audio_enabled,
                response: response_tx,
            })
            .await
            .map_err(|_| StartDisplaySetupError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| StartDisplaySetupError::ManagerStopped)?
    }

    pub async fn display_setup_operation(
        &self,
        display_id: CastDisplayId,
    ) -> Result<Option<DisplaySetupHandle>, ManagerRequestError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::GetDisplaySetupOperation {
                display_id,
                response: response_tx,
            })
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)
    }

    pub async fn cancel_display_setup(
        &self,
        display_id: CastDisplayId,
    ) -> Result<bool, ManagerRequestError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::CancelDisplaySetup {
                display_id,
                response: response_tx,
            })
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)
    }

    pub async fn forget_display_setup_operation(
        &self,
        display_id: CastDisplayId,
    ) -> Result<bool, ManagerRequestError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::ForgetDisplaySetupOperation {
                display_id,
                response: response_tx,
            })
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ManagerRequestError::ManagerStopped)
    }

    pub async fn remove_display(
        &self,
        display_id: CastDisplayId,
    ) -> Result<(), RemoveManagedDisplayError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::RemoveDisplay {
                display_id,
                response: response_tx,
            })
            .await
            .map_err(|_| RemoveManagedDisplayError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| RemoveManagedDisplayError::ManagerStopped)?
    }

    fn spawn_display_setup_operation(
        &self,
        selection: DeviceSelection,
        preferred_output: Option<CastKmsOutputId>,
        caller: PinnedCallerProcess,
        audio_enabled: bool,
    ) -> Result<DisplaySetupOperation, DisplaySetupStartError> {
        DisplaySetupOperation::spawn_pending(
            self.clone(),
            PendingDisplaySelection {
                selection,
                preferred_output,
            },
            caller,
            DisplaySetupDependencies::new(
                Arc::clone(&self.kernel_session_provider),
                Arc::clone(&self.pnp_resolver),
                self.media_runtime.clone(),
                initial_preparation_offer(
                    audio_enabled,
                    self.media_runtime.capture_source().initial_raw_layouts(),
                ),
                audio_enabled,
            ),
        )
    }
}

/// A manager-validated, one-shot route to the selected backend Device.
///
/// Consuming this value to create a session prevents callers from retaining a
/// seemingly current selection and replaying it later. The backend supervisor
/// independently checks its live connection and discovery generations before
/// any P2P call is made.
#[derive(Debug)]
pub struct ResolvedDeviceSelection {
    device: DeviceInfo,
    backend: BackendHandle,
}

/// A generation-validated Device paired with one manager-reserved CastKMS
/// output, before grant acquisition or any kernel/network side effect.
#[derive(Debug)]
pub struct ReservedCastDisplaySlot {
    device: DeviceInfo,
    selection_token: DeviceSelection,
    selection: Option<ResolvedDeviceSelection>,
    reservation: Option<OutputReservation>,
    releases: mpsc::UnboundedSender<OutputReservationRelease>,
    manager_commands: Option<mpsc::Sender<ManagerCommand>>,
}

/// Cloneable, target-bound route back to the manager's current exact Device.
///
/// It carries no output reservation and cannot select a different Device.  A
/// per-display recovery actor uses it only after inventory reports that this
/// same target is available again.
#[derive(Debug, Clone)]
pub(crate) struct DeviceSessionResolver {
    commands: mpsc::Sender<ManagerCommand>,
    backend_id: String,
    device_id: String,
}

impl DeviceSessionResolver {
    pub(crate) async fn resolve(
        &self,
        device: &DeviceInfo,
    ) -> Result<ResolvedDeviceSelection, ResolveDeviceError> {
        if device.backend_id != self.backend_id || device.device_id != self.device_id {
            return Err(ResolveDeviceError::InvalidSelection(
                "recovery Device differs from the reserved display target".into(),
            ));
        }
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(ManagerCommand::ResolveDevice {
                selection: DeviceSelection::from_device(device),
                response: response_tx,
            })
            .await
            .map_err(|_| ResolveDeviceError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ResolveDeviceError::ManagerStopped)?
    }
}

impl ReservedCastDisplaySlot {
    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub fn output(&self) -> &CastKmsOutput {
        self.reservation
            .as_ref()
            .expect("reserved slot still owns its reservation")
            .output()
    }

    pub(crate) fn take_selection(&mut self) -> Option<ResolvedDeviceSelection> {
        self.selection.take()
    }

    pub(crate) fn device_session_resolver(
        &self,
    ) -> Result<DeviceSessionResolver, ResolveDeviceError> {
        Ok(DeviceSessionResolver {
            commands: self
                .manager_commands
                .as_ref()
                .ok_or(ResolveDeviceError::ManagerStopped)?
                .clone(),
            backend_id: self.device.backend_id.clone(),
            device_id: self.device.device_id.clone(),
        })
    }

    pub(crate) async fn revalidate_device(&self) -> Result<(), ResolveDeviceError> {
        let commands = self
            .manager_commands
            .as_ref()
            .ok_or(ResolveDeviceError::ManagerStopped)?;
        let (response_tx, response_rx) = oneshot::channel();
        commands
            .send(ManagerCommand::ResolveDevice {
                selection: self.selection_token.clone(),
                response: response_tx,
            })
            .await
            .map_err(|_| ResolveDeviceError::ManagerStopped)?;
        response_rx
            .await
            .map_err(|_| ResolveDeviceError::ManagerStopped)??;
        Ok(())
    }
}

impl Drop for ReservedCastDisplaySlot {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            let _ = self.releases.send(reservation.release());
        }
    }
}

#[cfg(test)]
pub(crate) fn test_reserved_display_slot(
    device: DeviceInfo,
    output: CastKmsOutput,
) -> (
    ReservedCastDisplaySlot,
    mpsc::UnboundedReceiver<OutputReservationRelease>,
) {
    let mut pool = OutputSlotPool::default();
    let reservation = pool.reserve(&device, &[output], None).unwrap();
    let (releases, release_rx) = mpsc::unbounded_channel();
    (
        ReservedCastDisplaySlot {
            selection_token: DeviceSelection::from_device(&device),
            device,
            selection: None,
            reservation: Some(reservation),
            releases,
            manager_commands: None,
        },
        release_rx,
    )
}

impl ResolvedDeviceSelection {
    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub async fn create_session(
        self,
        session_id: impl Into<String>,
        session_generation: u64,
        requested_features: u64,
    ) -> Result<BackendSessionHandle, BackendSessionError> {
        let request = BackendSessionRequest::new(
            session_id,
            self.device.device_id,
            SessionOptions {
                connection_generation: self.device.connection_generation,
                discovery_generation: self.device.discovery_generation,
                session_generation,
                requested_features,
            },
        )?;
        self.backend.create_session(request).await
    }
}

#[derive(Debug)]
pub struct ManagerActor {
    handle: ManagerHandle,
    events: Option<mpsc::Receiver<InventoryEvent>>,
    lifecycle_events: Option<mpsc::UnboundedReceiver<LifecycleEvent>>,
    task: Option<JoinHandle<Result<ManagerShutdownReport, ManagerTaskError>>>,
}

impl ManagerActor {
    pub fn spawn(
        configs: Vec<BackendConfig>,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
    ) -> Result<Self, ManagerStartError> {
        Self::spawn_with_media_runtime(
            configs,
            kernel_session_provider,
            MediaRuntime::for_user(Uid::effective().as_raw()),
        )
    }

    pub fn spawn_with_media_runtime(
        configs: Vec<BackendConfig>,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
        media_runtime: MediaRuntime,
    ) -> Result<Self, ManagerStartError> {
        Self::spawn_with_output_provider_and_media_runtime(
            configs,
            Arc::new(SystemOutputInventoryProvider),
            kernel_session_provider,
            media_runtime,
        )
    }

    pub fn spawn_with_output_provider(
        configs: Vec<BackendConfig>,
        output_provider: Arc<dyn OutputInventoryProvider>,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
    ) -> Result<Self, ManagerStartError> {
        Self::spawn_with_output_provider_and_media_runtime(
            configs,
            output_provider,
            kernel_session_provider,
            MediaRuntime::for_user(Uid::effective().as_raw()),
        )
    }

    pub fn spawn_with_output_provider_and_media_runtime(
        configs: Vec<BackendConfig>,
        output_provider: Arc<dyn OutputInventoryProvider>,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
        media_runtime: MediaRuntime,
    ) -> Result<Self, ManagerStartError> {
        let pnp_resolver =
            PnpIdResolver::load_system(SYSTEM_PNP_IDS_PATH, &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .map_err(|error| ManagerStartError::LoadPnpDatabase(error.to_string()))?;
        Self::spawn_with_providers_and_media_runtime(
            configs,
            output_provider,
            kernel_session_provider,
            Arc::new(pnp_resolver),
            media_runtime,
        )
    }

    pub fn spawn_with_providers(
        configs: Vec<BackendConfig>,
        output_provider: Arc<dyn OutputInventoryProvider>,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
    ) -> Result<Self, ManagerStartError> {
        Self::spawn_with_providers_and_media_runtime(
            configs,
            output_provider,
            kernel_session_provider,
            pnp_resolver,
            MediaRuntime::for_user(Uid::effective().as_raw()),
        )
    }

    pub fn spawn_with_providers_and_media_runtime(
        configs: Vec<BackendConfig>,
        output_provider: Arc<dyn OutputInventoryProvider>,
        kernel_session_provider: Arc<dyn KernelSessionProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
        media_runtime: MediaRuntime,
    ) -> Result<Self, ManagerStartError> {
        if configs.len() > MAX_INSTALLED_BACKENDS {
            return Err(ManagerStartError::TooManyBackends(configs.len()));
        }
        let mut backend_ids = HashSet::with_capacity(configs.len());
        for config in &configs {
            if !backend_ids.insert(config.endpoint.backend_id().to_owned()) {
                return Err(ManagerStartError::DuplicateBackendId(
                    config.endpoint.backend_id().to_owned(),
                ));
            }
        }

        let (backend_event_tx, backend_event_rx) = mpsc::channel(BACKEND_EVENT_QUEUE);
        let mut workers = Vec::with_capacity(configs.len());
        for config in configs {
            let backend_id = config.endpoint.backend_id().to_owned();
            let supervisor = BackendSupervisor::spawn(
                config.endpoint,
                config.initial_connection_generation,
                config.registration_validator,
                config.reconnect_policy,
            )
            .map_err(|source| ManagerStartError::StartBackend {
                backend_id: backend_id.clone(),
                source,
            })?;
            workers.push(BackendWorker::spawn(
                backend_id,
                supervisor,
                backend_event_tx.clone(),
            ));
        }
        drop(backend_event_tx);

        let (command_tx, command_rx) = mpsc::channel(MANAGER_COMMAND_QUEUE);
        let (reservation_release_tx, reservation_release_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel(MANAGER_EVENT_QUEUE);
        let (lifecycle_event_tx, lifecycle_event_rx) = mpsc::unbounded_channel();
        let (slot_event_tx, slot_event_rx) = mpsc::unbounded_channel();
        let handle = ManagerHandle {
            commands: command_tx,
            output_provider,
            kernel_session_provider,
            pnp_resolver,
            media_runtime,
        };
        let task = tokio::spawn(run_manager(ManagerTaskContext {
            commands: command_rx,
            events: ManagerEventSinks {
                inventory: event_tx,
                lifecycle: lifecycle_event_tx,
            },
            backend_events: backend_event_rx,
            reservation_releases: reservation_release_tx,
            reservation_release_events: reservation_release_rx,
            slot_events: slot_event_tx,
            slot_event_rx,
            manager: handle.clone(),
            workers,
        }));
        Ok(Self {
            handle,
            events: Some(event_rx),
            lifecycle_events: Some(lifecycle_event_rx),
            task: Some(task),
        })
    }

    pub fn handle(&self) -> ManagerHandle {
        self.handle.clone()
    }

    pub fn take_events(&mut self) -> Option<mpsc::Receiver<InventoryEvent>> {
        self.events.take()
    }

    pub fn take_lifecycle_events(&mut self) -> Option<mpsc::UnboundedReceiver<LifecycleEvent>> {
        self.lifecycle_events.take()
    }

    pub async fn shutdown(mut self) -> Result<ManagerShutdownReport, ManagerActorError> {
        let deadline = Instant::now() + MANAGER_SHUTDOWN_TIMEOUT;
        let response = if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            None
        } else {
            let (response_tx, response_rx) = oneshot::channel();
            timeout_at(
                deadline,
                self.handle
                    .commands
                    .send(ManagerCommand::Shutdown(response_tx)),
            )
            .await
            .map_err(|_| ManagerActorError::ShutdownTimeout)?
            .map_err(|_| ManagerActorError::Stopped)?;
            Some(response_rx)
        };

        let report = match response {
            Some(response) => timeout_at(deadline, response)
                .await
                .map_err(|_| ManagerActorError::ShutdownTimeout)?
                .map_err(|_| ManagerActorError::Stopped)?,
            None => self.join_task_until(deadline).await?,
        };
        if self.task.is_some() {
            let joined = self.join_task_until(deadline).await?;
            debug_assert_eq!(joined, report);
        }
        Ok(report)
    }

    async fn join_task_until(
        &mut self,
        deadline: Instant,
    ) -> Result<ManagerShutdownReport, ManagerActorError> {
        let Some(mut task) = self.task.take() else {
            return Err(ManagerActorError::Stopped);
        };
        if task.is_finished() {
            return task
                .await
                .map_err(ManagerActorError::Task)?
                .map_err(ManagerActorError::Failed);
        }
        match timeout_at(deadline, &mut task).await {
            Ok(result) => result
                .map_err(ManagerActorError::Task)?
                .map_err(ManagerActorError::Failed),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(ManagerActorError::ShutdownTimeout)
            }
        }
    }
}

impl Drop for ManagerActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // `shutdown` is the orderly path. Never detach the root resource
            // owner if that path times out or its command queue is saturated.
            task.abort();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerShutdownReport {
    pub backend_reports: BTreeMap<String, BackendShutdownReport>,
    pub errors: BTreeMap<String, String>,
}

#[derive(Debug)]
enum ManagerCommand {
    ListDevices(oneshot::Sender<DeviceSnapshot>),
    ListDisplays(oneshot::Sender<Vec<AddedCastDisplaySnapshot>>),
    GetDisplay {
        display_id: CastDisplayId,
        response: oneshot::Sender<Option<AddedCastDisplaySnapshot>>,
    },
    ResolveDevice {
        selection: DeviceSelection,
        response: oneshot::Sender<Result<ResolvedDeviceSelection, ResolveDeviceError>>,
    },
    ReserveDisplaySlot {
        selection: DeviceSelection,
        outputs: Vec<CastKmsOutput>,
        preferred_output: Option<CastKmsOutputId>,
        response: oneshot::Sender<Result<ReservedCastDisplaySlot, ReserveDisplaySlotError>>,
    },
    StartDisplaySetup {
        selection: DeviceSelection,
        preferred_output: Option<CastKmsOutputId>,
        caller: PinnedCallerProcess,
        audio_enabled: bool,
        response: oneshot::Sender<Result<DisplaySetupHandle, StartDisplaySetupError>>,
    },
    GetDisplaySetupOperation {
        display_id: CastDisplayId,
        response: oneshot::Sender<Option<DisplaySetupHandle>>,
    },
    CancelDisplaySetup {
        display_id: CastDisplayId,
        response: oneshot::Sender<bool>,
    },
    ForgetDisplaySetupOperation {
        display_id: CastDisplayId,
        response: oneshot::Sender<bool>,
    },
    RemoveDisplay {
        display_id: CastDisplayId,
        response: oneshot::Sender<Result<(), RemoveManagedDisplayError>>,
    },
    Shutdown(oneshot::Sender<ManagerShutdownReport>),
}

struct ManagerTaskContext {
    commands: mpsc::Receiver<ManagerCommand>,
    events: ManagerEventSinks,
    backend_events: mpsc::Receiver<BackendWorkerMessage>,
    reservation_releases: mpsc::UnboundedSender<OutputReservationRelease>,
    reservation_release_events: mpsc::UnboundedReceiver<OutputReservationRelease>,
    slot_events: mpsc::UnboundedSender<CastDisplaySlotEvent>,
    slot_event_rx: mpsc::UnboundedReceiver<CastDisplaySlotEvent>,
    manager: ManagerHandle,
    workers: Vec<BackendWorker>,
}

async fn run_manager(
    context: ManagerTaskContext,
) -> Result<ManagerShutdownReport, ManagerTaskError> {
    let ManagerTaskContext {
        mut commands,
        events,
        mut backend_events,
        reservation_releases,
        reservation_release_events: mut reservation_release_rx,
        slot_events,
        mut slot_event_rx,
        manager,
        workers,
    } = context;
    let mut inventory = AggregateInventory::default();
    let mut output_slots = OutputSlotPool::default();
    let mut records = BTreeMap::<CastDisplayId, ManagedDisplayRecord>::new();
    let mut setup_tasks = JoinSet::<SetupCompletion>::new();
    let mut setup_task_ids = HashMap::new();
    let mut removal_tasks = JoinSet::<RemovalCompletion>::new();
    let mut removal_task_ids = HashMap::new();
    let mut shutdown_response = None;
    let mut backend_events_open = true;

    loop {
        tokio::select! {
            // Public requests must not starve setup, removal, backend, or
            // display-slot progress. Tokio's default randomized branch order
            // gives every continuously-ready input a chance to run.
            Some(release) = reservation_release_rx.recv() => {
                if !output_slots.release(&release) {
                    debug!(?release, "ignored stale display-slot release");
                }
            },
            command = commands.recv() => match command {
                Some(ManagerCommand::ListDevices(response)) => {
                    let _ = response.send(inventory.snapshot());
                }
                Some(ManagerCommand::ListDisplays(response)) => {
                    let snapshots = records.values().filter_map(|record| match &record.phase {
                        ManagedDisplayPhase::Active(display) => Some(display.snapshot()),
                        _ => None,
                    }).collect();
                    let _ = response.send(snapshots);
                }
                Some(ManagerCommand::GetDisplay { display_id, response }) => {
                    let snapshot = records.get(&display_id).and_then(|record| match &record.phase {
                        ManagedDisplayPhase::Active(display) => Some(display.snapshot()),
                        _ => None,
                    });
                    let _ = response.send(snapshot);
                }
                Some(ManagerCommand::ResolveDevice { selection, response }) => {
                    let result = inventory.resolve_device(&selection).and_then(|device| {
                        let backend = workers
                            .iter()
                            .find(|worker| worker.backend_id == device.backend_id)
                            .map(|worker| worker.handle.clone())
                            .ok_or_else(|| ResolveDeviceError::BackendUnavailable {
                                backend_id: device.backend_id.clone(),
                            })?;
                        Ok(ResolvedDeviceSelection { device, backend })
                    });
                    let _ = response.send(result);
                }
                Some(ManagerCommand::ReserveDisplaySlot {
                    selection,
                    outputs,
                    preferred_output,
                    response,
                }) => {
                    let result = (|| {
                        let device = inventory.resolve_device(&selection)?;
                        let backend = workers
                            .iter()
                            .find(|worker| worker.backend_id == device.backend_id)
                            .map(|worker| worker.handle.clone())
                            .ok_or_else(|| ResolveDeviceError::BackendUnavailable {
                                backend_id: device.backend_id.clone(),
                            })?;
                        let reservation = output_slots.reserve_where(
                            &device,
                            &outputs,
                            preferred_output.as_ref(),
                            |output| manager.kernel_session_provider.may_acquire(output),
                        )?;
                        Ok(ReservedCastDisplaySlot {
                            device: device.clone(),
                            selection_token: selection,
                            selection: Some(ResolvedDeviceSelection { device, backend }),
                            reservation: Some(reservation),
                            releases: reservation_releases.clone(),
                            manager_commands: None,
                        })
                    })();
                    let _ = response.send(result);
                }
                Some(ManagerCommand::StartDisplaySetup {
                    selection,
                    preferred_output,
                    caller,
                    audio_enabled,
                    response,
                }) => {
                    let result = start_managed_display_setup(
                        &manager,
                        selection,
                        preferred_output,
                        caller,
                        audio_enabled,
                        &mut records,
                        &mut setup_tasks,
                        &mut setup_task_ids,
                    );
                    let _ = response.send(result);
                }
                Some(ManagerCommand::GetDisplaySetupOperation { display_id, response }) => {
                    let handle = records.get(&display_id).map(|record| record.handle.clone());
                    let _ = response.send(handle);
                }
                Some(ManagerCommand::CancelDisplaySetup { display_id, response }) => {
                    let cancelled = records.get(&display_id).is_some_and(|record| {
                        if record.handle.snapshot().stage.is_terminal() {
                            false
                        } else {
                            record.handle.cancel();
                            true
                        }
                    });
                    let _ = response.send(cancelled);
                }
                Some(ManagerCommand::ForgetDisplaySetupOperation { display_id, response }) => {
                    let forgettable = records.get(&display_id).is_some_and(|record| {
                        record.handle.snapshot().stage.is_terminal()
                            && matches!(record.phase, ManagedDisplayPhase::Terminal)
                    });
                    if forgettable {
                        records.remove(&display_id);
                    }
                    let _ = response.send(forgettable);
                }
                Some(ManagerCommand::RemoveDisplay { display_id, response }) => {
                    match records.get_mut(&display_id) {
                        Some(record) if matches!(record.phase, ManagedDisplayPhase::Active(_)) => {
                            let previous = std::mem::replace(
                                &mut record.phase,
                                ManagedDisplayPhase::Removing { waiters: vec![response] },
                            );
                            let ManagedDisplayPhase::Active(display) = previous else { unreachable!() };
                            let abort = removal_tasks.spawn(async move {
                                RemovalCompletion {
                                    display_id,
                                    result: display
                                        .remove(DeviceSessionStopReason::DisplayRemoved)
                                        .await
                                        .map_err(|error| error.to_string()),
                                }
                            });
                            removal_task_ids.insert(abort.id(), display_id);
                        }
                        Some(ManagedDisplayRecord { phase: ManagedDisplayPhase::Removing { waiters }, .. }) => {
                            waiters.push(response);
                        }
                        _ => {
                            // Remove is idempotent, including after successful cleanup.
                            let _ = response.send(Ok(()));
                        }
                    }
                }
                Some(ManagerCommand::Shutdown(response)) => {
                    shutdown_response = Some(response);
                    break;
                }
                None => break,
            },
            joined = setup_tasks.join_next_with_id(), if !setup_tasks.is_empty() => {
                if let Some(event) = handle_setup_join(
                    joined.expect("nonempty setup JoinSet returned no task"),
                    &mut setup_task_ids,
                    &mut records,
                    &inventory,
                    &slot_events,
                ) {
                    let _ = events.lifecycle.send(event);
                }
            },
            joined = removal_tasks.join_next_with_id(), if !removal_tasks.is_empty() => {
                if let Some(event) = handle_removal_join(
                    joined.expect("nonempty removal JoinSet returned no task"),
                    &mut removal_task_ids,
                    &mut records,
                ) {
                    let _ = events.lifecycle.send(event);
                }
            },
            message = backend_events.recv(), if backend_events_open => match message {
                Some(BackendWorkerMessage::Event { backend_id, event }) => {
                    match inventory.apply_supervisor_event(&backend_id, &event) {
                        Ok(ApplySupervisorOutcome::Changed(changes)) => {
                            publish_inventory_changes(changes, &records, &events).await?;
                        }
                        Ok(ApplySupervisorOutcome::IgnoredStale) => {
                            debug!(backend_id, ?event, "ignored stale backend event");
                        }
                        Err(error) => {
                            warn!(backend_id, %error, "rejected backend inventory event");
                        }
                    }
                }
                Some(BackendWorkerMessage::Stopped { backend_id, error }) => {
                    warn!(backend_id, error, "backend supervisor stopped unexpectedly");
                    let changes = inventory.mark_backend_unavailable(&backend_id)?;
                    publish_inventory_changes(changes, &records, &events).await?;
                }
                None => {
                    backend_events_open = false;
                    let changes = inventory.mark_all_unavailable()?;
                    publish_inventory_changes(changes, &records, &events).await?;
                }
            },
            event = slot_event_rx.recv() => {
                match event {
                    Some(CastDisplaySlotEvent::StateChanged(snapshot)) => {
                        if records.get(&snapshot.display_id).is_some_and(|record| matches!(record.phase, ManagedDisplayPhase::Active(_))) {
                            let _ = events.lifecycle.send(LifecycleEvent::DisplayStateChanged(snapshot));
                        }
                    }
                    Some(CastDisplaySlotEvent::TerminalFailure {
                        display_id,
                        error,
                        cleanup_error,
                    }) => {
                        if let Some(record) = records.get_mut(&display_id) {
                            let previous = std::mem::replace(&mut record.phase, ManagedDisplayPhase::Terminal);
                            let ManagedDisplayPhase::Active(display) = previous else {
                                record.phase = previous;
                                continue;
                            };
                            if let Err(join_error) = display.join_after_terminal().await {
                                warn!(%display_id, %join_error, "could not reap terminal cast-display owner");
                            }
                            if let Some(cleanup_error) = cleanup_error {
                                warn!(%display_id, %error, %cleanup_error, "removing cast display after terminal failure left cleanup errors");
                            } else {
                                warn!(%display_id, %error, "removed cast display after terminal failure");
                            }
                            let _ = events
                                .lifecycle
                                .send(LifecycleEvent::DisplayRemoved { display_id });
                        }
                    }
                    None => {}
                }
            },
        }
    }

    commands.close();
    for record in records.values() {
        if !record.handle.snapshot().stage.is_terminal() {
            record.handle.cancel();
        }
    }
    while let Some(joined) = setup_tasks.join_next_with_id().await {
        handle_setup_join(
            joined,
            &mut setup_task_ids,
            &mut records,
            &inventory,
            &slot_events,
        );
    }
    while let Some(joined) = removal_tasks.join_next_with_id().await {
        handle_removal_join(joined, &mut removal_task_ids, &mut records);
    }

    let mut display_cleanup_errors = BTreeMap::new();
    let mut shutdown_removals = JoinSet::new();
    for (display_id, record) in records {
        if let ManagedDisplayPhase::Active(display) = record.phase {
            shutdown_removals.spawn(async move {
                (
                    display_id,
                    display
                        .remove(DeviceSessionStopReason::DaemonShutdown)
                        .await
                        .map_err(|error| error.to_string()),
                )
            });
        }
    }
    while let Some(joined) = shutdown_removals.join_next().await {
        match joined {
            Ok((display_id, Err(error))) => {
                display_cleanup_errors.insert(format!("display:{display_id}"), error);
            }
            Ok((_, Ok(()))) => {}
            Err(error) => {
                display_cleanup_errors.insert(
                    format!("display-cleanup-task:{}", error.id()),
                    error.to_string(),
                );
            }
        }
    }

    let mut report = shutdown_workers(workers).await;
    report.errors.extend(display_cleanup_errors);
    if let Some(response) = shutdown_response {
        let _ = response.send(report.clone());
    }
    Ok(report)
}

async fn publish_inventory_changes(
    changes: Vec<InventoryEvent>,
    records: &BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    events: &ManagerEventSinks,
) -> Result<(), ManagerTaskError> {
    for change in changes {
        events
            .inventory
            .send(change.clone())
            .await
            .map_err(|_| ManagerTaskError::EventConsumerStopped)?;
        refresh_configured_displays(records, &change).await;
    }
    Ok(())
}

async fn refresh_configured_displays(
    records: &BTreeMap<CastDisplayId, ManagedDisplayRecord>,
    event: &InventoryEvent,
) {
    for display in records.values().filter_map(|record| match &record.phase {
        ManagedDisplayPhase::Active(display) => Some(display),
        _ => None,
    }) {
        let handle = display.handle();
        let current = handle.snapshot().device;
        let Some(device) = configured_device_update(&current, event) else {
            continue;
        };
        if let Err(error) = handle.update_device(device).await {
            warn!(display_id = %handle.display_id(), %error, "failed to refresh configured Device state");
        }
    }
}

#[derive(Debug, Error)]
pub enum ManagerStartError {
    #[error("load the trusted PNP manufacturer database: {0}")]
    LoadPnpDatabase(String),
    #[error("configured {0} backends; limit is {MAX_INSTALLED_BACKENDS}")]
    TooManyBackends(usize),
    #[error("backend ID {0:?} is configured twice")]
    DuplicateBackendId(String),
    #[error("start backend {backend_id:?}: {source}")]
    StartBackend {
        backend_id: String,
        source: BackendSupervisorError,
    },
}

#[derive(Debug, Error)]
pub enum StartDisplaySetupError {
    #[error("Pronk manager has stopped")]
    ManagerStopped,
    #[error("invalid Device selection: {0}")]
    InvalidSelection(String),
    #[error("the setup-operation retention limit is exhausted")]
    TooManyOperations,
    #[error("generated a duplicate cast-display identity")]
    IdentityCollision,
    #[error("start display setup operation: {0}")]
    Start(#[source] DisplaySetupStartError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoveManagedDisplayError {
    #[error("Pronk manager has stopped")]
    ManagerStopped,
    #[error("remove cast display: {0}")]
    Cleanup(String),
}

#[derive(Debug, Error)]
pub enum ManagerRequestError {
    #[error("Pronk manager has stopped")]
    ManagerStopped,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolveDeviceError {
    #[error("Pronk manager has stopped")]
    ManagerStopped,
    #[error("invalid device selection: {0}")]
    InvalidSelection(String),
    #[error("device {backend_id:?}/{device_id:?} was not found")]
    NotFound {
        backend_id: String,
        device_id: String,
    },
    #[error("device {backend_id:?}/{device_id:?} changed since it was selected")]
    StaleSelection {
        backend_id: String,
        device_id: String,
    },
    #[error("device {backend_id:?}/{device_id:?} is {availability}")]
    Unavailable {
        backend_id: String,
        device_id: String,
        availability: DeviceAvailability,
    },
    #[error("backend {backend_id:?} is unavailable")]
    BackendUnavailable { backend_id: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReserveDisplaySlotError {
    #[error("Pronk manager has stopped")]
    ManagerStopped,
    #[error("cannot reserve a display for the selected Device: {0}")]
    Device(#[from] ResolveDeviceError),
    #[error("CastKMS output discovery task failed: {0}")]
    DiscoveryTask(String),
    #[error("discover CastKMS outputs: {0}")]
    Discovery(#[from] OutputInventoryProviderError),
    #[error("cannot reserve a CastKMS output: {0}")]
    Output(#[from] OutputReservationError),
}

#[derive(Debug, Error)]
pub enum ManagerActorError {
    #[error("Pronk manager has stopped")]
    Stopped,
    #[error("Pronk manager shutdown timed out")]
    ShutdownTimeout,
    #[error("Pronk manager task failed: {0}")]
    Task(tokio::task::JoinError),
    #[error("Pronk manager failed: {0}")]
    Failed(ManagerTaskError),
}

#[derive(Debug, Error)]
pub enum ManagerTaskError {
    #[error("inventory signal consumer stopped")]
    EventConsumerStopped,
    #[error("aggregate inventory failed: {0}")]
    Aggregate(String),
}

impl From<AggregateError> for ManagerTaskError {
    fn from(error: AggregateError) -> Self {
        Self::Aggregate(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::test_support::UnreachableKernelSessionProvider;

    #[tokio::test]
    async fn manager_with_no_backends_lists_and_stops_cleanly() {
        let actor =
            ManagerActor::spawn(Vec::new(), Arc::new(UnreachableKernelSessionProvider)).unwrap();
        assert_eq!(
            actor.handle().list_devices().await.unwrap(),
            DeviceSnapshot {
                inventory_revision: 0,
                devices: Vec::new(),
            }
        );
        let report = actor.shutdown().await.unwrap();
        assert!(report.backend_reports.is_empty());
        assert!(report.errors.is_empty());
    }

    #[derive(Debug)]
    struct CountingOutputProvider(Arc<AtomicUsize>);

    impl OutputInventoryProvider for CountingOutputProvider {
        fn discover(&self) -> Result<Vec<CastKmsOutput>, OutputInventoryProviderError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn invalid_device_selection_never_touches_drm_discovery() {
        let calls = Arc::new(AtomicUsize::new(0));
        let actor = ManagerActor::spawn_with_output_provider(
            Vec::new(),
            Arc::new(CountingOutputProvider(Arc::clone(&calls))),
            Arc::new(UnreachableKernelSessionProvider),
        )
        .unwrap();
        let selection = DeviceSelection {
            backend_id: "mock".into(),
            device_id: "missing".into(),
            connection_generation: 1,
            discovery_generation: 1,
            device_revision: 1,
        };
        assert!(matches!(
            actor.handle().reserve_display_slot(selection, None).await,
            Err(ReserveDisplaySlotError::Device(
                ResolveDeviceError::NotFound { .. }
            ))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        actor.shutdown().await.unwrap();
    }
}
