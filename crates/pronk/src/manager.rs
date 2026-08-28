use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use pronk_backend_host::{
    BackendEndpoint, BackendHandle, BackendReconnectPolicy, BackendRegistrationValidator,
    BackendSessionError, BackendSessionHandle, BackendSessionRequest, BackendShutdownReport,
    BackendSupervisor, BackendSupervisorError, BackendSupervisorEvent, DeviceInventorySnapshot,
    MAX_INSTALLED_BACKENDS,
};
use pronk_backend_protocol::{DeviceAvailability as BackendAvailability, SessionOptions, Validate};
use pronk_core::grant::GrantProvider;
use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID, SYSTEM_PNP_IDS_PATH};
use pronk_core::output::{
    discover_castkms_outputs, CastKmsOutput, CastKmsOutputId, OutputDiscoveryError,
};
use pronk_core::session::PinnedCallerSession;
use pronk_dbus::{
    DeviceAvailability, DeviceInfo, DeviceSelection, DeviceSnapshot, DiscoveryMetadataEntry,
    MAX_PUBLIC_DEVICES,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at, Instant};
use tracing::{debug, warn};

use crate::cast_display_slot::{CastDisplaySlotActor, CastDisplaySlotEvent};
use crate::device_session_port::DeviceSessionStopReason;
use crate::display::{
    AddedCastDisplay, AddedCastDisplaySnapshot, CastDisplayId, DisplaySetupHandle,
    DisplaySetupOperation, DisplaySetupOperationError, DisplaySetupStartError,
    PendingDisplaySelection,
};
use crate::preparation::initial_preparation_offer;
use crate::slot::{
    OutputReservation, OutputReservationError, OutputReservationRelease, OutputSlotPool,
};

const MANAGER_COMMAND_QUEUE: usize = 32;
const MANAGER_EVENT_QUEUE: usize = 256;
const BACKEND_EVENT_QUEUE: usize = 256;
const MANAGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETAINED_SETUP_OPERATIONS: usize = 128;

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
    grant_provider: Arc<dyn GrantProvider>,
    pnp_resolver: Arc<PnpIdResolver>,
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
    /// The caller identity must already have been obtained from the session
    /// bus broker and pidfd-pinned. No grant policy or DRM target is accepted
    /// from the public client. The returned handle observes and explicitly
    /// cancels the operation; dropping it does not cancel manager-owned work.
    pub async fn start_display_setup(
        &self,
        selection: DeviceSelection,
        preferred_output: Option<CastKmsOutputId>,
        caller: PinnedCallerSession,
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
        caller: PinnedCallerSession,
        audio_enabled: bool,
    ) -> Result<DisplaySetupOperation, DisplaySetupStartError> {
        DisplaySetupOperation::spawn_pending(
            self.clone(),
            PendingDisplaySelection {
                selection,
                preferred_output,
            },
            caller,
            Arc::clone(&self.grant_provider),
            Arc::clone(&self.pnp_resolver),
            initial_preparation_offer(audio_enabled),
            audio_enabled,
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
        grant_provider: Arc<dyn GrantProvider>,
    ) -> Result<Self, ManagerStartError> {
        Self::spawn_with_output_provider(
            configs,
            Arc::new(SystemOutputInventoryProvider),
            grant_provider,
        )
    }

    pub fn spawn_with_output_provider(
        configs: Vec<BackendConfig>,
        output_provider: Arc<dyn OutputInventoryProvider>,
        grant_provider: Arc<dyn GrantProvider>,
    ) -> Result<Self, ManagerStartError> {
        let pnp_resolver =
            PnpIdResolver::load_system(SYSTEM_PNP_IDS_PATH, &[], DEFAULT_SYNTHESIZER_PNP_ID)
                .map_err(|error| ManagerStartError::LoadPnpDatabase(error.to_string()))?;
        Self::spawn_with_providers(
            configs,
            output_provider,
            grant_provider,
            Arc::new(pnp_resolver),
        )
    }

    pub fn spawn_with_providers(
        configs: Vec<BackendConfig>,
        output_provider: Arc<dyn OutputInventoryProvider>,
        grant_provider: Arc<dyn GrantProvider>,
        pnp_resolver: Arc<PnpIdResolver>,
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
            grant_provider,
            pnp_resolver,
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
        caller: PinnedCallerSession,
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

#[derive(Debug)]
struct ManagedSetupOperation {
    target: DeviceTarget,
    handle: DisplaySetupHandle,
}

#[derive(Debug)]
struct SetupCompletion {
    display_id: CastDisplayId,
    result: Result<AddedCastDisplay, DisplaySetupOperationError>,
}

#[derive(Debug)]
struct RemovalCompletion {
    display_id: CastDisplayId,
    target: DeviceTarget,
    result: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DeviceTarget {
    backend_id: String,
    device_id: String,
}

impl From<&DeviceSelection> for DeviceTarget {
    fn from(selection: &DeviceSelection) -> Self {
        Self {
            backend_id: selection.backend_id.clone(),
            device_id: selection.device_id.clone(),
        }
    }
}

impl From<&DeviceInfo> for DeviceTarget {
    fn from(device: &DeviceInfo) -> Self {
        Self {
            backend_id: device.backend_id.clone(),
            device_id: device.device_id.clone(),
        }
    }
}

#[derive(Debug)]
enum BackendWorkerMessage {
    Event {
        backend_id: String,
        event: BackendSupervisorEvent,
    },
    Stopped {
        backend_id: String,
        error: String,
    },
}

struct BackendWorker {
    backend_id: String,
    handle: BackendHandle,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<BackendShutdownReport, BackendSupervisorError>>,
}

impl BackendWorker {
    fn spawn(
        backend_id: String,
        supervisor: BackendSupervisor,
        events: mpsc::Sender<BackendWorkerMessage>,
    ) -> Self {
        let handle = supervisor.handle();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task_backend_id = backend_id.clone();
        let task = tokio::spawn(run_backend_worker(
            task_backend_id,
            supervisor,
            events,
            shutdown_rx,
        ));
        Self {
            backend_id,
            handle,
            shutdown: Some(shutdown_tx),
            task,
        }
    }
}

async fn run_backend_worker(
    backend_id: String,
    mut supervisor: BackendSupervisor,
    events: mpsc::Sender<BackendWorkerMessage>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<BackendShutdownReport, BackendSupervisorError> {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => return supervisor.shutdown().await,
            event = supervisor.next_event() => match event {
                Some(event) => {
                    let message = BackendWorkerMessage::Event {
                        backend_id: backend_id.clone(),
                        event,
                    };
                    tokio::select! {
                        biased;
                        _ = &mut shutdown => return supervisor.shutdown().await,
                        result = events.send(message) => {
                            if result.is_err() {
                                return supervisor.shutdown().await;
                            }
                        }
                    }
                }
                None => {
                    let _ = events.send(BackendWorkerMessage::Stopped {
                        backend_id,
                        error: "backend supervisor event stream closed".into(),
                    }).await;
                    return Err(BackendSupervisorError::SupervisorStopped);
                }
            },
        }
    }
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
    let mut operations = BTreeMap::<CastDisplayId, ManagedSetupOperation>::new();
    let mut target_owners = BTreeMap::<DeviceTarget, CastDisplayId>::new();
    let mut displays = BTreeMap::<CastDisplayId, CastDisplaySlotActor>::new();
    let mut setup_tasks = JoinSet::<SetupCompletion>::new();
    let mut setup_task_ids = HashMap::new();
    let mut removal_tasks = JoinSet::<RemovalCompletion>::new();
    let mut removal_task_ids = HashMap::new();
    let mut removal_waiters = BTreeMap::<
        CastDisplayId,
        Vec<oneshot::Sender<Result<(), RemoveManagedDisplayError>>>,
    >::new();
    let mut shutdown_response = None;
    let mut backend_events_open = true;

    loop {
        tokio::select! {
            biased;
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
                    let snapshots = displays.values().map(CastDisplaySlotActor::snapshot).collect();
                    let _ = response.send(snapshots);
                }
                Some(ManagerCommand::GetDisplay { display_id, response }) => {
                    let snapshot = displays.get(&display_id).map(CastDisplaySlotActor::snapshot);
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
                        let reservation = output_slots.reserve(
                            &device,
                            &outputs,
                            preferred_output.as_ref(),
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
                        &mut operations,
                        &mut target_owners,
                        &mut setup_tasks,
                        &mut setup_task_ids,
                    );
                    let _ = response.send(result);
                }
                Some(ManagerCommand::GetDisplaySetupOperation { display_id, response }) => {
                    let handle = operations.get(&display_id).map(|operation| operation.handle.clone());
                    let _ = response.send(handle);
                }
                Some(ManagerCommand::CancelDisplaySetup { display_id, response }) => {
                    let cancelled = operations.get(&display_id).is_some_and(|operation| {
                        if operation.handle.snapshot().stage.is_terminal() {
                            false
                        } else {
                            operation.handle.cancel();
                            true
                        }
                    });
                    let _ = response.send(cancelled);
                }
                Some(ManagerCommand::ForgetDisplaySetupOperation { display_id, response }) => {
                    let forgettable = operations.get(&display_id).is_some_and(|operation| {
                        operation.handle.snapshot().stage.is_terminal()
                            && target_owners.get(&operation.target) != Some(&display_id)
                            && !displays.contains_key(&display_id)
                            && !removal_waiters.contains_key(&display_id)
                    });
                    if forgettable {
                        operations.remove(&display_id);
                    }
                    let _ = response.send(forgettable);
                }
                Some(ManagerCommand::RemoveDisplay { display_id, response }) => {
                    if let Some(waiters) = removal_waiters.get_mut(&display_id) {
                        waiters.push(response);
                    } else if let Some(display) = displays.remove(&display_id) {
                        let target = DeviceTarget::from(&display.snapshot().device);
                        removal_waiters.insert(display_id, vec![response]);
                        let completion_target = target.clone();
                        let abort = removal_tasks.spawn(async move {
                            RemovalCompletion {
                                display_id,
                                target: completion_target,
                                result: display
                                    .remove(DeviceSessionStopReason::DisplayRemoved)
                                    .await
                                    .map_err(|error| error.to_string()),
                            }
                        });
                        removal_task_ids.insert(abort.id(), (display_id, target));
                    } else {
                        // Remove is deliberately idempotent, including after a
                        // previous successful cleanup.
                        let _ = response.send(Ok(()));
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
                    &mut operations,
                    &mut target_owners,
                    &mut displays,
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
                    &mut removal_waiters,
                    &mut target_owners,
                    &mut operations,
                ) {
                    let _ = events.lifecycle.send(event);
                }
            },
            message = backend_events.recv(), if backend_events_open => match message {
                Some(BackendWorkerMessage::Event { backend_id, event }) => {
                    match inventory.apply_supervisor_event(&backend_id, &event) {
                        Ok(ApplySupervisorOutcome::Changed(changes)) => {
                            publish_inventory_changes(changes, &mut displays, &events).await?;
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
                    publish_inventory_changes(changes, &mut displays, &events).await?;
                }
                None => {
                    backend_events_open = false;
                    let changes = inventory.mark_all_unavailable()?;
                    publish_inventory_changes(changes, &mut displays, &events).await?;
                }
            },
            event = slot_event_rx.recv() => {
                match event {
                    Some(CastDisplaySlotEvent::StateChanged(snapshot)) => {
                        if displays.contains_key(&snapshot.display_id) {
                            let _ = events.lifecycle.send(LifecycleEvent::DisplayStateChanged(snapshot));
                        }
                    }
                    Some(CastDisplaySlotEvent::TerminalFailure {
                        display_id,
                        error,
                        cleanup_error,
                    }) => {
                        if let Some(display) = displays.remove(&display_id) {
                            let target = DeviceTarget::from(&display.snapshot().device);
                            if let Err(join_error) = display.join_after_terminal().await {
                                warn!(%display_id, %join_error, "could not reap terminal cast-display owner");
                            }
                            release_target_owner(&mut target_owners, &target, display_id);
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
    for operation in operations.values() {
        if !operation.handle.snapshot().stage.is_terminal() {
            operation.handle.cancel();
        }
    }
    while let Some(joined) = setup_tasks.join_next_with_id().await {
        handle_setup_join(
            joined,
            &mut setup_task_ids,
            &mut operations,
            &mut target_owners,
            &mut displays,
            &inventory,
            &slot_events,
        );
    }
    while let Some(joined) = removal_tasks.join_next_with_id().await {
        handle_removal_join(
            joined,
            &mut removal_task_ids,
            &mut removal_waiters,
            &mut target_owners,
            &mut operations,
        );
    }

    let mut display_cleanup_errors = BTreeMap::new();
    let mut shutdown_removals = JoinSet::new();
    for (display_id, display) in displays {
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

#[allow(clippy::too_many_arguments)]
fn start_managed_display_setup(
    manager: &ManagerHandle,
    selection: DeviceSelection,
    preferred_output: Option<CastKmsOutputId>,
    caller: PinnedCallerSession,
    audio_enabled: bool,
    operations: &mut BTreeMap<CastDisplayId, ManagedSetupOperation>,
    target_owners: &mut BTreeMap<DeviceTarget, CastDisplayId>,
    setup_tasks: &mut JoinSet<SetupCompletion>,
    setup_task_ids: &mut HashMap<tokio::task::Id, CastDisplayId>,
) -> Result<DisplaySetupHandle, StartDisplaySetupError> {
    selection
        .validate()
        .map_err(|error| StartDisplaySetupError::InvalidSelection(error.to_string()))?;
    let target = DeviceTarget::from(&selection);
    if let Some(display_id) = target_owners.get(&target) {
        return operations
            .get(display_id)
            .map(|operation| operation.handle.clone())
            .ok_or(StartDisplaySetupError::InconsistentTarget);
    }

    if operations.len() >= MAX_RETAINED_SETUP_OPERATIONS {
        return Err(StartDisplaySetupError::TooManyOperations);
    }

    let operation = manager
        .spawn_display_setup_operation(selection, preferred_output, caller, audio_enabled)
        .map_err(StartDisplaySetupError::Start)?;
    let display_id = operation.display_id();
    let handle = operation.handle();
    if operations.contains_key(&display_id) {
        return Err(StartDisplaySetupError::IdentityCollision);
    }
    if target_owners.contains_key(&target) {
        return Err(StartDisplaySetupError::InconsistentTarget);
    }
    operations.insert(
        display_id,
        ManagedSetupOperation {
            target: target.clone(),
            handle: handle.clone(),
        },
    );
    target_owners.insert(target, display_id);
    let abort = setup_tasks.spawn(async move {
        SetupCompletion {
            display_id,
            result: operation.finish().await,
        }
    });
    setup_task_ids.insert(abort.id(), display_id);
    Ok(handle)
}

fn handle_setup_join(
    joined: Result<(tokio::task::Id, SetupCompletion), tokio::task::JoinError>,
    setup_task_ids: &mut HashMap<tokio::task::Id, CastDisplayId>,
    operations: &mut BTreeMap<CastDisplayId, ManagedSetupOperation>,
    target_owners: &mut BTreeMap<DeviceTarget, CastDisplayId>,
    displays: &mut BTreeMap<CastDisplayId, CastDisplaySlotActor>,
    inventory: &AggregateInventory,
    slot_events: &mpsc::UnboundedSender<CastDisplaySlotEvent>,
) -> Option<LifecycleEvent> {
    let completion = match joined {
        Ok((task_id, completion)) => {
            setup_task_ids.remove(&task_id);
            completion
        }
        Err(error) => {
            let display_id = setup_task_ids.remove(&error.id());
            if let Some(display_id) = display_id {
                if let Some(operation) = operations.remove(&display_id) {
                    release_target_owner(target_owners, &operation.target, display_id);
                }
            }
            warn!(%error, ?display_id, "manager-owned display setup task failed");
            return None;
        }
    };
    let Some(operation) = operations.get(&completion.display_id) else {
        warn!(display_id = %completion.display_id, "completed display setup has no manager record");
        return None;
    };
    match completion.result {
        Ok(mut display) => {
            debug_assert_eq!(display.display_id(), completion.display_id);
            display.update_device(inventory.configured_device(display.device()));
            let actor = match CastDisplaySlotActor::spawn(display, slot_events.clone()) {
                Ok(actor) => actor,
                Err(error) => {
                    let target = operation.target.clone();
                    release_target_owner(target_owners, &target, completion.display_id);
                    warn!(display_id = %completion.display_id, %error, "failed to start cast-display slot actor");
                    return None;
                }
            };
            let snapshot = actor.snapshot();
            if displays.insert(completion.display_id, actor).is_some() {
                warn!(display_id = %completion.display_id, "replaced duplicate added display");
            }
            Some(LifecycleEvent::DisplayAdded(Box::new(snapshot)))
        }
        Err(error) => {
            let target = operation.target.clone();
            release_target_owner(target_owners, &target, completion.display_id);
            debug!(display_id = %completion.display_id, %error, "display setup reached a terminal non-added state");
            None
        }
    }
}

async fn publish_inventory_changes(
    changes: Vec<InventoryEvent>,
    displays: &mut BTreeMap<CastDisplayId, CastDisplaySlotActor>,
    events: &ManagerEventSinks,
) -> Result<(), ManagerTaskError> {
    for change in changes {
        events
            .inventory
            .send(change.clone())
            .await
            .map_err(|_| ManagerTaskError::EventConsumerStopped)?;
        refresh_configured_displays(displays, &change).await;
    }
    Ok(())
}

async fn refresh_configured_displays(
    displays: &BTreeMap<CastDisplayId, CastDisplaySlotActor>,
    event: &InventoryEvent,
) {
    for display in displays.values() {
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

fn configured_device_update(current: &DeviceInfo, event: &InventoryEvent) -> Option<DeviceInfo> {
    match event {
        InventoryEvent::DeviceAdded { device, .. }
        | InventoryEvent::DeviceChanged { device, .. }
            if current.backend_id == device.backend_id && current.device_id == device.device_id =>
        {
            Some(device.clone())
        }
        InventoryEvent::DeviceRemoved {
            inventory_revision,
            backend_id,
            device_id,
        } if current.backend_id == *backend_id && current.device_id == *device_id => {
            let mut unavailable = current.clone();
            unavailable.availability = DeviceAvailability::Unavailable;
            unavailable.device_revision = *inventory_revision;
            Some(unavailable)
        }
        _ => None,
    }
}

fn handle_removal_join(
    joined: Result<(tokio::task::Id, RemovalCompletion), tokio::task::JoinError>,
    removal_task_ids: &mut HashMap<tokio::task::Id, (CastDisplayId, DeviceTarget)>,
    removal_waiters: &mut BTreeMap<
        CastDisplayId,
        Vec<oneshot::Sender<Result<(), RemoveManagedDisplayError>>>,
    >,
    target_owners: &mut BTreeMap<DeviceTarget, CastDisplayId>,
    operations: &mut BTreeMap<CastDisplayId, ManagedSetupOperation>,
) -> Option<LifecycleEvent> {
    let (display_id, target, result) = match joined {
        Ok((task_id, completion)) => {
            removal_task_ids.remove(&task_id);
            (completion.display_id, completion.target, completion.result)
        }
        Err(error) => {
            let Some((display_id, target)) = removal_task_ids.remove(&error.id()) else {
                warn!(%error, "unidentified manager-owned display removal task failed");
                return None;
            };
            (display_id, target, Err(error.to_string()))
        }
    };
    release_target_owner(target_owners, &target, display_id);
    operations.remove(&display_id);
    let response = result.map_err(RemoveManagedDisplayError::Cleanup);
    if let Some(waiters) = removal_waiters.remove(&display_id) {
        for waiter in waiters {
            let _ = waiter.send(response.clone());
        }
    }
    Some(LifecycleEvent::DisplayRemoved { display_id })
}

fn release_target_owner(
    target_owners: &mut BTreeMap<DeviceTarget, CastDisplayId>,
    target: &DeviceTarget,
    display_id: CastDisplayId,
) {
    if target_owners.get(target) == Some(&display_id) {
        target_owners.remove(target);
    }
}

async fn shutdown_workers(mut workers: Vec<BackendWorker>) -> ManagerShutdownReport {
    for worker in &mut workers {
        if let Some(shutdown) = worker.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    let mut backend_reports = BTreeMap::new();
    let mut errors = BTreeMap::new();
    let wait = async {
        for worker in &mut workers {
            match (&mut worker.task).await {
                Ok(Ok(report)) => {
                    backend_reports.insert(worker.backend_id.clone(), report);
                }
                Ok(Err(error)) => {
                    errors.insert(worker.backend_id.clone(), error.to_string());
                }
                Err(error) => {
                    errors.insert(worker.backend_id.clone(), error.to_string());
                }
            }
        }
    };
    if timeout(MANAGER_SHUTDOWN_TIMEOUT, wait).await.is_err() {
        for worker in &workers {
            if !worker.task.is_finished() {
                worker.task.abort();
                errors.insert(worker.backend_id.clone(), "shutdown timed out".into());
            }
        }
    }
    ManagerShutdownReport {
        backend_reports,
        errors,
    }
}

#[derive(Debug, Default)]
struct AggregateInventory {
    inventory_revision: u64,
    backends: BTreeMap<String, BackendInventory>,
}

#[derive(Debug, Default)]
struct BackendInventory {
    connection_generation: u64,
    discovery_generation: Option<u64>,
    devices: BTreeMap<String, DeviceInfo>,
}

#[derive(Debug, PartialEq, Eq)]
enum ApplySupervisorOutcome {
    Changed(Vec<InventoryEvent>),
    IgnoredStale,
}

impl AggregateInventory {
    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            inventory_revision: self.inventory_revision,
            devices: self
                .backends
                .values()
                .flat_map(|backend| backend.devices.values().cloned())
                .collect(),
        }
    }

    fn configured_device(&self, previous: &DeviceInfo) -> DeviceInfo {
        if let Some(current) = self
            .backends
            .get(&previous.backend_id)
            .and_then(|backend| backend.devices.get(&previous.device_id))
        {
            return current.clone();
        }

        // Configured displays outlive passive discovery records. Preserve the
        // last bounded identity, but make absence explicit and advance its
        // token to the inventory revision that already observed the removal.
        let mut unavailable = previous.clone();
        unavailable.availability = DeviceAvailability::Unavailable;
        unavailable.device_revision = self.inventory_revision.max(previous.device_revision);
        unavailable
    }

    fn resolve_device(
        &self,
        selection: &DeviceSelection,
    ) -> Result<DeviceInfo, ResolveDeviceError> {
        selection
            .validate()
            .map_err(|error| ResolveDeviceError::InvalidSelection(error.to_string()))?;
        let device = self
            .backends
            .get(&selection.backend_id)
            .and_then(|backend| backend.devices.get(&selection.device_id))
            .ok_or_else(|| ResolveDeviceError::NotFound {
                backend_id: selection.backend_id.clone(),
                device_id: selection.device_id.clone(),
            })?;
        if device.connection_generation != selection.connection_generation
            || device.discovery_generation != selection.discovery_generation
            || device.device_revision != selection.device_revision
        {
            return Err(ResolveDeviceError::StaleSelection {
                backend_id: selection.backend_id.clone(),
                device_id: selection.device_id.clone(),
            });
        }
        if device.availability != DeviceAvailability::Available {
            return Err(ResolveDeviceError::Unavailable {
                backend_id: selection.backend_id.clone(),
                device_id: selection.device_id.clone(),
                availability: device.availability,
            });
        }
        Ok(device.clone())
    }

    fn apply_supervisor_event(
        &mut self,
        backend_id: &str,
        event: &BackendSupervisorEvent,
    ) -> Result<ApplySupervisorOutcome, AggregateError> {
        match event {
            BackendSupervisorEvent::Connecting {
                connection_generation,
            }
            | BackendSupervisorEvent::ConnectionFailed {
                connection_generation,
                ..
            } => self.advance_connection(backend_id, *connection_generation),
            BackendSupervisorEvent::Connected {
                connection_generation,
                inventory,
                ..
            } => self.replace_backend(backend_id, *connection_generation, inventory),
            BackendSupervisorEvent::InventoryChanged {
                connection_generation,
                inventory,
            }
            | BackendSupervisorEvent::InventoryResynchronized {
                connection_generation,
                inventory,
                ..
            }
            | BackendSupervisorEvent::Disconnected {
                connection_generation,
                unavailable_inventory: inventory,
                ..
            } => {
                let current = self
                    .backends
                    .get(backend_id)
                    .map_or(0, |backend| backend.connection_generation);
                if *connection_generation < current {
                    return Ok(ApplySupervisorOutcome::IgnoredStale);
                }
                if *connection_generation > current {
                    return Err(AggregateError::UnexpectedConnectionGeneration {
                        backend_id: backend_id.into(),
                        expected: current,
                        actual: *connection_generation,
                    });
                }
                self.replace_backend(backend_id, *connection_generation, inventory)
            }
            BackendSupervisorEvent::Stopped {
                last_connection_generation,
            } => {
                if let Some(last) = last_connection_generation {
                    let current = self
                        .backends
                        .get(backend_id)
                        .map_or(0, |backend| backend.connection_generation);
                    if *last < current {
                        return Ok(ApplySupervisorOutcome::IgnoredStale);
                    }
                }
                self.mark_backend_unavailable(backend_id)
                    .map(ApplySupervisorOutcome::Changed)
            }
            BackendSupervisorEvent::ReconnectScheduled { .. }
            | BackendSupervisorEvent::ReconnectExhausted { .. } => {
                Ok(ApplySupervisorOutcome::Changed(Vec::new()))
            }
        }
    }

    fn advance_connection(
        &mut self,
        backend_id: &str,
        connection_generation: u64,
    ) -> Result<ApplySupervisorOutcome, AggregateError> {
        let current = self
            .backends
            .get(backend_id)
            .map_or(0, |backend| backend.connection_generation);
        if connection_generation <= current {
            return Ok(ApplySupervisorOutcome::IgnoredStale);
        }
        let changes = self.mark_backend_unavailable(backend_id)?;
        let backend = self.backends.entry(backend_id.into()).or_default();
        backend.connection_generation = connection_generation;
        backend.discovery_generation = None;
        Ok(ApplySupervisorOutcome::Changed(changes))
    }

    fn replace_backend(
        &mut self,
        backend_id: &str,
        connection_generation: u64,
        snapshot: &DeviceInventorySnapshot,
    ) -> Result<ApplySupervisorOutcome, AggregateError> {
        let current = self
            .backends
            .get(backend_id)
            .map_or(0, |backend| backend.connection_generation);
        if connection_generation < current {
            return Ok(ApplySupervisorOutcome::IgnoredStale);
        }
        if connection_generation == 0
            || snapshot.discovery_generation == 0
            || snapshot.revision == 0
        {
            return Err(AggregateError::ZeroGeneration);
        }

        let old_devices = self
            .backends
            .get(backend_id)
            .map_or_else(BTreeMap::new, |backend| backend.devices.clone());
        let mut new_devices = BTreeMap::new();
        for device in &snapshot.devices {
            device
                .validate()
                .map_err(|error| AggregateError::InvalidDevice(error.to_string()))?;
            if device.backend_id != backend_id {
                return Err(AggregateError::WrongBackendId {
                    expected: backend_id.into(),
                    actual: device.backend_id.clone(),
                });
            }
            let existing = old_devices.get(&device.device_id);
            let public = public_device(
                device,
                connection_generation,
                snapshot.discovery_generation,
                existing,
            );
            if new_devices
                .insert(device.device_id.clone(), public)
                .is_some()
            {
                return Err(AggregateError::DuplicateDevice {
                    backend_id: backend_id.into(),
                    device_id: device.device_id.clone(),
                });
            }
        }

        let old_total = self.device_count();
        let new_total = old_total - old_devices.len() + new_devices.len();
        if new_total > MAX_PUBLIC_DEVICES {
            return Err(AggregateError::TooManyDevices(new_total));
        }

        let changes = diff_devices(&old_devices, &new_devices);
        self.ensure_revision_capacity(changes.len())?;
        let backend = self.backends.entry(backend_id.into()).or_default();
        backend.connection_generation = connection_generation;
        backend.discovery_generation = Some(snapshot.discovery_generation);
        backend.devices = new_devices;
        Ok(ApplySupervisorOutcome::Changed(
            self.revision_events(backend_id, changes),
        ))
    }

    fn mark_backend_unavailable(
        &mut self,
        backend_id: &str,
    ) -> Result<Vec<InventoryEvent>, AggregateError> {
        let Some(backend) = self.backends.get(backend_id) else {
            return Ok(Vec::new());
        };
        let mut new_devices = backend.devices.clone();
        for device in new_devices.values_mut() {
            if device.availability != DeviceAvailability::Unavailable {
                device.availability = DeviceAvailability::Unavailable;
                device.device_revision = 0;
            }
        }
        let changes = diff_devices(&backend.devices, &new_devices);
        self.ensure_revision_capacity(changes.len())?;
        self.backends
            .get_mut(backend_id)
            .expect("backend disappeared")
            .devices = new_devices;
        Ok(self.revision_events(backend_id, changes))
    }

    fn mark_all_unavailable(&mut self) -> Result<Vec<InventoryEvent>, AggregateError> {
        let backend_ids: Vec<_> = self.backends.keys().cloned().collect();
        let required = self
            .backends
            .values()
            .flat_map(|backend| backend.devices.values())
            .filter(|device| device.availability != DeviceAvailability::Unavailable)
            .count();
        self.ensure_revision_capacity(required)?;
        let mut events = Vec::with_capacity(required);
        for backend_id in backend_ids {
            events.extend(self.mark_backend_unavailable(&backend_id)?);
        }
        Ok(events)
    }

    fn device_count(&self) -> usize {
        self.backends
            .values()
            .map(|backend| backend.devices.len())
            .sum()
    }

    fn ensure_revision_capacity(&self, changes: usize) -> Result<(), AggregateError> {
        self.inventory_revision
            .checked_add(changes as u64)
            .ok_or(AggregateError::RevisionExhausted)
            .map(|_| ())
    }

    fn revision_events(
        &mut self,
        backend_id: &str,
        changes: Vec<DeviceChange>,
    ) -> Vec<InventoryEvent> {
        let mut events = Vec::with_capacity(changes.len());
        for change in changes {
            self.inventory_revision = self
                .inventory_revision
                .checked_add(1)
                .expect("revision capacity checked");
            let event = match change {
                DeviceChange::Added(mut device) => {
                    device.device_revision = self.inventory_revision;
                    self.backends
                        .get_mut(backend_id)
                        .and_then(|backend| backend.devices.get_mut(&device.device_id))
                        .expect("added device disappeared")
                        .device_revision = device.device_revision;
                    InventoryEvent::DeviceAdded {
                        inventory_revision: self.inventory_revision,
                        device,
                    }
                }
                DeviceChange::Changed(mut device) => {
                    device.device_revision = self.inventory_revision;
                    self.backends
                        .get_mut(backend_id)
                        .and_then(|backend| backend.devices.get_mut(&device.device_id))
                        .expect("changed device disappeared")
                        .device_revision = device.device_revision;
                    InventoryEvent::DeviceChanged {
                        inventory_revision: self.inventory_revision,
                        device,
                    }
                }
                DeviceChange::Removed(device_id) => InventoryEvent::DeviceRemoved {
                    inventory_revision: self.inventory_revision,
                    backend_id: backend_id.into(),
                    device_id,
                },
            };
            events.push(event);
        }
        events
    }
}

#[derive(Debug)]
enum DeviceChange {
    Added(DeviceInfo),
    Changed(DeviceInfo),
    Removed(String),
}

fn diff_devices(
    old: &BTreeMap<String, DeviceInfo>,
    new: &BTreeMap<String, DeviceInfo>,
) -> Vec<DeviceChange> {
    let mut changes = Vec::new();
    for device_id in old.keys() {
        if !new.contains_key(device_id) {
            changes.push(DeviceChange::Removed(device_id.clone()));
        }
    }
    for (device_id, device) in new {
        match old.get(device_id) {
            None => changes.push(DeviceChange::Added(device.clone())),
            Some(previous) if previous != device => {
                changes.push(DeviceChange::Changed(device.clone()));
            }
            Some(_) => {}
        }
    }
    changes
}

fn public_device(
    device: &pronk_backend_protocol::DeviceInfo,
    connection_generation: u64,
    discovery_generation: u64,
    existing: Option<&DeviceInfo>,
) -> DeviceInfo {
    let availability = match device.availability {
        BackendAvailability::Available => DeviceAvailability::Available,
        BackendAvailability::Busy => DeviceAvailability::Busy,
        BackendAvailability::Unavailable => DeviceAvailability::Unavailable,
    };
    let metadata: Vec<_> = device
        .metadata
        .iter()
        .map(|entry| DiscoveryMetadataEntry {
            key: entry.key.clone(),
            value: entry.value.clone(),
        })
        .collect();
    let materially_unchanged = existing.is_some_and(|existing| {
        existing.backend_id == device.backend_id
            && existing.device_id == device.device_id
            && existing.display_name == device.display_name
            && existing.availability == availability
            && existing.connection_generation == connection_generation
            && existing.discovery_generation == discovery_generation
            && existing.metadata == metadata
    });
    let device_revision = if materially_unchanged {
        existing.expect("checked above").device_revision
    } else {
        // Filled with the core-owned global revision when the change is
        // committed. The manager task cannot expose this transient value.
        0
    };
    DeviceInfo {
        backend_id: device.backend_id.clone(),
        device_id: device.device_id.clone(),
        display_name: device.display_name.clone(),
        availability,
        connection_generation,
        discovery_generation,
        device_revision,
        metadata,
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
    #[error("the manager's Device-to-display ownership table is inconsistent")]
    InconsistentTarget,
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

#[derive(Debug, Error, PartialEq, Eq)]
enum AggregateError {
    #[error("device inventory generation/revision must be nonzero")]
    ZeroGeneration,
    #[error("invalid device: {0}")]
    InvalidDevice(String),
    #[error("device backend ID {actual:?} does not match source {expected:?}")]
    WrongBackendId { expected: String, actual: String },
    #[error("duplicate device {backend_id:?}/{device_id:?}")]
    DuplicateDevice {
        backend_id: String,
        device_id: String,
    },
    #[error("aggregate contains {0} devices; limit is {MAX_PUBLIC_DEVICES}")]
    TooManyDevices(usize),
    #[error("public inventory revision is exhausted")]
    RevisionExhausted,
    #[error(
        "backend {backend_id:?} event has connection generation {actual}; expected {expected}"
    )]
    UnexpectedConnectionGeneration {
        backend_id: String,
        expected: u64,
        actual: u64,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pronk_backend_protocol::{
        BackendInfo, DeviceAvailability as BackendAvailability, DeviceInfo as BackendDevice,
    };

    use super::*;
    use crate::test_support::UnreachableGrantProvider;

    fn backend_device(backend_id: &str, device_id: &str, name: &str) -> BackendDevice {
        BackendDevice {
            backend_id: backend_id.into(),
            device_id: device_id.into(),
            display_name: name.into(),
            availability: BackendAvailability::Available,
            metadata: Vec::new(),
        }
    }

    fn inventory(
        discovery_generation: u64,
        revision: u64,
        devices: Vec<BackendDevice>,
    ) -> DeviceInventorySnapshot {
        DeviceInventorySnapshot {
            discovery_generation,
            revision,
            devices,
        }
    }

    fn connected(
        connection_generation: u64,
        inventory: DeviceInventorySnapshot,
    ) -> BackendSupervisorEvent {
        BackendSupervisorEvent::Connected {
            connection_generation,
            negotiated_minor: 0,
            info: BackendInfo::v1("mock", "Mock", "test", "mock", "development"),
            inventory,
        }
    }

    fn changed(
        connection_generation: u64,
        inventory: DeviceInventorySnapshot,
    ) -> BackendSupervisorEvent {
        BackendSupervisorEvent::InventoryChanged {
            connection_generation,
            inventory,
        }
    }

    #[test]
    fn aggregates_backends_with_one_ordered_public_revision() {
        let mut aggregate = AggregateInventory::default();
        let first = aggregate
            .apply_supervisor_event(
                "alpha",
                &connected(
                    1,
                    inventory(10, 2, vec![backend_device("alpha", "one", "One")]),
                ),
            )
            .unwrap();
        assert!(matches!(
            first,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceAdded { inventory_revision: 1, .. }])
        ));
        aggregate
            .apply_supervisor_event(
                "beta",
                &connected(
                    1,
                    inventory(20, 4, vec![backend_device("beta", "two", "Two")]),
                ),
            )
            .unwrap();

        let snapshot = aggregate.snapshot();
        assert_eq!(snapshot.inventory_revision, 2);
        assert_eq!(snapshot.devices.len(), 2);
        assert_eq!(snapshot.devices[0].backend_id, "alpha");
        assert_eq!(snapshot.devices[1].backend_id, "beta");
        snapshot.validate().unwrap();
    }

    #[test]
    fn disconnect_and_reconnect_preserve_identity_with_new_generations() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    7,
                    inventory(
                        3,
                        2,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        let mut unavailable = inventory(
            3,
            2,
            vec![backend_device("mock", "living-room", "Living Room")],
        );
        unavailable.devices[0].availability = BackendAvailability::Unavailable;
        let disconnect = BackendSupervisorEvent::Disconnected {
            connection_generation: 7,
            reason: pronk_backend_host::BackendDisconnectReason::ConnectionClosed,
            unavailable_inventory: unavailable,
        };
        assert!(matches!(
            aggregate
                .apply_supervisor_event("mock", &disconnect)
                .unwrap(),
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceChanged { inventory_revision: 2, device }]
                    if device.device_revision == 2
                        && device.availability == DeviceAvailability::Unavailable)
        ));

        aggregate
            .apply_supervisor_event(
                "mock",
                &BackendSupervisorEvent::Connecting {
                    connection_generation: 8,
                },
            )
            .unwrap();
        assert_eq!(
            aggregate
                .apply_supervisor_event("mock", &changed(7, inventory(3, 3, Vec::new())))
                .unwrap(),
            ApplySupervisorOutcome::IgnoredStale
        );
        let reconnect = aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    8,
                    inventory(
                        4,
                        2,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        assert!(matches!(
            reconnect,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceChanged { inventory_revision: 3, device }]
                    if device.connection_generation == 8
                        && device.discovery_generation == 4
                        && device.device_revision == 3
                        && device.availability == DeviceAvailability::Available)
        ));
    }

    #[test]
    fn unchanged_devices_keep_their_exact_device_revision() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    1,
                    inventory(
                        1,
                        2,
                        vec![
                            backend_device("mock", "one", "One"),
                            backend_device("mock", "two", "Two"),
                        ],
                    ),
                ),
            )
            .unwrap();
        let outcome = aggregate
            .apply_supervisor_event(
                "mock",
                &changed(
                    1,
                    inventory(
                        1,
                        3,
                        vec![
                            backend_device("mock", "one", "One renamed"),
                            backend_device("mock", "two", "Two"),
                        ],
                    ),
                ),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceChanged { device, .. }]
                    if device.device_id == "one" && device.device_revision == 3)
        ));
        let snapshot = aggregate.snapshot();
        assert_eq!(snapshot.devices[0].device_revision, 3);
        assert_eq!(snapshot.devices[1].device_revision, 2);
    }

    #[test]
    fn readded_identity_gets_a_fresh_device_revision() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    1,
                    inventory(
                        1,
                        1,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        assert_eq!(aggregate.snapshot().devices[0].device_revision, 1);

        aggregate
            .apply_supervisor_event("mock", &changed(1, inventory(1, 2, Vec::new())))
            .unwrap();
        let readded = aggregate
            .apply_supervisor_event(
                "mock",
                &changed(
                    1,
                    inventory(
                        1,
                        3,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        assert!(matches!(
            readded,
            ApplySupervisorOutcome::Changed(ref events)
                if matches!(events.as_slice(), [InventoryEvent::DeviceAdded { inventory_revision: 3, device }]
                    if device.device_revision == 3)
        ));
    }

    #[test]
    fn configured_device_state_survives_removal_and_tracks_readdition() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    1,
                    inventory(
                        1,
                        1,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        let current = aggregate.snapshot().devices.remove(0);
        let removal = aggregate
            .apply_supervisor_event("mock", &changed(1, inventory(1, 2, Vec::new())))
            .unwrap();
        let ApplySupervisorOutcome::Changed(removal_events) = removal else {
            panic!("Device removal was ignored");
        };
        let unavailable = configured_device_update(&current, &removal_events[0]).unwrap();
        assert_eq!(unavailable.display_name, "Living Room");
        assert_eq!(unavailable.availability, DeviceAvailability::Unavailable);
        assert_eq!(unavailable.device_revision, 2);
        assert_eq!(aggregate.configured_device(&current), unavailable);

        let readded = aggregate
            .apply_supervisor_event(
                "mock",
                &changed(
                    1,
                    inventory(2, 3, vec![backend_device("mock", "living-room", "Den TV")]),
                ),
            )
            .unwrap();
        let ApplySupervisorOutcome::Changed(readded_events) = readded else {
            panic!("Device readdition was ignored");
        };
        let available = configured_device_update(&unavailable, &readded_events[0]).unwrap();
        assert_eq!(available.display_name, "Den TV");
        assert_eq!(available.availability, DeviceAvailability::Available);
        assert_eq!(available.discovery_generation, 2);
        assert_eq!(available.device_revision, 3);

        let unrelated = InventoryEvent::DeviceRemoved {
            inventory_revision: 4,
            backend_id: "mock".into(),
            device_id: "bedroom".into(),
        };
        assert!(configured_device_update(&available, &unrelated).is_none());
    }

    #[test]
    fn aggregate_bound_rejects_a_whole_snapshot_atomically() {
        let mut aggregate = AggregateInventory::default();
        let full: Vec<_> = (0..MAX_PUBLIC_DEVICES)
            .map(|index| backend_device("alpha", &format!("device-{index}"), "Device"))
            .collect();
        aggregate
            .apply_supervisor_event("alpha", &connected(1, inventory(1, 1, full)))
            .unwrap();
        let before = aggregate.snapshot();
        assert_eq!(
            aggregate
                .apply_supervisor_event(
                    "beta",
                    &connected(
                        1,
                        inventory(1, 1, vec![backend_device("beta", "extra", "Extra")]),
                    ),
                )
                .unwrap_err(),
            AggregateError::TooManyDevices(MAX_PUBLIC_DEVICES + 1)
        );
        assert_eq!(aggregate.snapshot(), before);
    }

    #[test]
    fn resolves_only_the_exact_available_device_revision() {
        let mut aggregate = AggregateInventory::default();
        aggregate
            .apply_supervisor_event(
                "mock",
                &connected(
                    7,
                    inventory(
                        11,
                        1,
                        vec![backend_device("mock", "living-room", "Living Room")],
                    ),
                ),
            )
            .unwrap();
        let device = aggregate.snapshot().devices.remove(0);
        let selection = DeviceSelection::from_device(&device);
        assert_eq!(aggregate.resolve_device(&selection).unwrap(), device);

        for stale in [
            DeviceSelection {
                connection_generation: 8,
                ..selection.clone()
            },
            DeviceSelection {
                discovery_generation: 12,
                ..selection.clone()
            },
            DeviceSelection {
                device_revision: 2,
                ..selection.clone()
            },
        ] {
            assert!(matches!(
                aggregate.resolve_device(&stale),
                Err(ResolveDeviceError::StaleSelection { .. })
            ));
        }

        let mut unavailable = inventory(
            11,
            2,
            vec![backend_device("mock", "living-room", "Living Room")],
        );
        unavailable.devices[0].availability = BackendAvailability::Busy;
        aggregate
            .apply_supervisor_event("mock", &changed(7, unavailable))
            .unwrap();
        let current = DeviceSelection::from_device(&aggregate.snapshot().devices[0]);
        assert!(matches!(
            aggregate.resolve_device(&current),
            Err(ResolveDeviceError::Unavailable {
                availability: DeviceAvailability::Busy,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn manager_with_no_backends_lists_and_stops_cleanly() {
        let actor = ManagerActor::spawn(Vec::new(), Arc::new(UnreachableGrantProvider)).unwrap();
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
            Arc::new(UnreachableGrantProvider),
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
