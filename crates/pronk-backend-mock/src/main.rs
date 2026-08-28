use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{ensure, Context};
use futures_util::StreamExt;
use pronk_backend_protocol::{
    backend_peer_builder, require_same_uid, session_object_path, validate_media_configuration,
    BackendHost1Proxy, BackendInfo, ControlOperation, DeviceAvailability, DeviceCapabilities,
    DeviceIdentity, DeviceInfo, DeviceSnapshot, DiscoveryMetadataEntry, DisplayIdentity,
    DisplayMode, IdentitySource, MediaConfiguration, PipeWireTarget, PreparationRequest,
    RegistrationReply, SessionOptions, SessionState, SessionStatistics, StopReason, SuspendReason,
    Validate, BACKEND_PATH, SESSION_FEATURE_AUDIO, SESSION_FEATURE_CONTROL,
};
use pronk_systemd::{notify_ready, notify_stopping, take_backend_control_fd, BackendPeerPolicy};
use tokio::runtime::Builder;
use tokio::sync::{watch, Mutex as AsyncMutex};
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use zbus::MessageStream;

const MOCK_BACKEND_ID: &str = "mock";
const MAX_MOCK_MODES: usize = 3;

mod mock_media;

use mock_media::{MockMediaEngine, MockMediaError, MockMediaMode};

fn main() -> anyhow::Result<()> {
    // LISTEN_* must be consumed and unset before Tokio or any other thread is
    // created. Ambient PipeWire selection must likewise not survive startup.
    let control = take_backend_control_fd().context("take backend control fd")?;
    scrub_ambient_pipewire_environment();
    let stream = control.into_std_stream();
    let info = backend_info_from_environment()?;
    let discovery_scenario = DiscoveryScenario::from_environment()?;
    let peer_policy = BackendPeerPolicy::from_environment()?;
    let media_mode = MockMediaMode::from_environment().context("select mock media engine")?;
    ensure!(
        media_mode != MockMediaMode::RetainForProtocolTest || peer_policy.is_unmanaged_test(),
        "retain-for-protocol-test media mode requires unmanaged test peer policy"
    );

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?;
    runtime.block_on(run(
        stream,
        info,
        discovery_scenario,
        peer_policy,
        media_mode,
    ))
}

async fn run(
    stream: std::os::unix::net::UnixStream,
    info: BackendInfo,
    discovery_scenario: DiscoveryScenario,
    peer_policy: BackendPeerPolicy,
    media_mode: MockMediaMode,
) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::from_std(stream)
        .context("adopt activated backend control stream")?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let backend = MockBackend::new(info.clone(), shutdown_tx, discovery_scenario, media_mode);
    let connection = backend_peer_builder(stream)
        .serve_at(BACKEND_PATH, backend.clone())
        .context("export Backend1")?
        .build()
        .await
        .context("authenticate private P2P D-Bus client")?;
    require_same_uid(&connection)
        .await
        .context("authenticate Pronk peer UID")?;
    peer_policy
        .validate(&connection)
        .await
        .context("validate Pronk peer service identity")?;

    let host = BackendHost1Proxy::new(&connection)
        .await
        .context("create BackendHost1 proxy")?;
    let reply: RegistrationReply = host
        .register_backend(info)
        .await
        .context("register backend")?;
    reply.validate().context("validate registration reply")?;
    backend.set_connection_generation(reply.connection_generation);

    notify_ready().context("notify systemd that mock backend is ready")?;
    let mut messages = MessageStream::from(&connection);
    let requested_shutdown = loop {
        if *shutdown_rx.borrow() {
            break true;
        }
        tokio::select! {
            changed = shutdown_rx.changed() => {
                changed.context("Backend1 shutdown channel closed")?;
            }
            message = messages.next() => match message {
                None | Some(Err(zbus::Error::InputOutput(_))) => break false,
                Some(Err(error)) => return Err(error).context("read private P2P D-Bus connection"),
                Some(Ok(_)) => {}
            }
        }
    };
    notify_stopping().context("notify systemd that mock backend is stopping")?;
    let close = connection.close().await;
    if requested_shutdown {
        close.context("close private P2P D-Bus connection")?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct MockBackend {
    shared: Arc<MockBackendShared>,
}

#[derive(Debug)]
struct MockBackendShared {
    info: BackendInfo,
    discovery: Mutex<DiscoveryState>,
    connection_generation: AtomicU64,
    active_session: Mutex<Option<String>>,
    shutdown_tx: watch::Sender<bool>,
    discovery_scenario: DiscoveryScenario,
    media_mode: MockMediaMode,
}

#[derive(Debug, Default)]
struct DiscoveryState {
    next_generation: u64,
    active_generation: Option<u64>,
    revision: u64,
    living_room_renamed: bool,
}

impl MockBackend {
    fn new(
        info: BackendInfo,
        shutdown_tx: watch::Sender<bool>,
        discovery_scenario: DiscoveryScenario,
        media_mode: MockMediaMode,
    ) -> Self {
        Self {
            shared: Arc::new(MockBackendShared {
                info,
                discovery: Mutex::new(DiscoveryState::default()),
                connection_generation: AtomicU64::new(0),
                active_session: Mutex::new(None),
                shutdown_tx,
                discovery_scenario,
                media_mode,
            }),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        let living_room_name = if self
            .shared
            .discovery
            .lock()
            .expect("discovery mutex poisoned")
            .living_room_renamed
        {
            "Living Room TV (updated)"
        } else {
            "Living Room TV"
        };
        [
            ("living-room", living_room_name, "television"),
            ("office-display", "Office Display", "display"),
        ]
        .into_iter()
        .map(|(device_id, display_name, kind)| DeviceInfo {
            backend_id: self.shared.info.backend_id.clone(),
            device_id: device_id.into(),
            display_name: display_name.into(),
            availability: DeviceAvailability::Available,
            metadata: vec![DiscoveryMetadataEntry {
                key: "device-kind".into(),
                value: kind.into(),
            }],
        })
        .collect()
    }

    fn active_generation(&self) -> zbus::fdo::Result<u64> {
        self.shared
            .discovery
            .lock()
            .expect("discovery mutex poisoned")
            .active_generation
            .ok_or_else(|| zbus::fdo::Error::Failed("discovery is not active".into()))
    }

    fn set_connection_generation(&self, generation: u64) {
        self.shared
            .connection_generation
            .store(generation, Ordering::Release);
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk.Backend1")]
impl MockBackend {
    fn get_info(&self) -> BackendInfo {
        self.shared.info.clone()
    }

    async fn start_discovery(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<u64> {
        let generation = {
            let mut discovery = self
                .shared
                .discovery
                .lock()
                .expect("discovery mutex poisoned");
            if let Some(generation) = discovery.active_generation {
                return Ok(generation);
            }
            discovery.next_generation = discovery
                .next_generation
                .checked_add(1)
                .ok_or_else(|| zbus::fdo::Error::Failed("discovery generation exhausted".into()))?;
            discovery.active_generation = Some(discovery.next_generation);
            discovery.revision = 2;
            discovery.living_room_renamed = false;
            discovery.next_generation
        };

        // Emit the initial inventory before returning. A correct core has
        // already subscribed, then installs ListDevices() and discards these
        // queued events because their revisions are in the snapshot.
        for (index, device) in self.devices().into_iter().enumerate() {
            Self::device_added(&emitter, generation, index as u64 + 1, device)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        match self.shared.discovery_scenario {
            DiscoveryScenario::Stable => {}
            DiscoveryScenario::RevisionGap => {
                let backend = self.clone();
                let emitter = emitter.to_owned();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    {
                        let mut discovery = backend
                            .shared
                            .discovery
                            .lock()
                            .expect("discovery mutex poisoned");
                        if discovery.active_generation != Some(generation) {
                            return;
                        }
                        discovery.revision = 4;
                        discovery.living_room_renamed = true;
                    }
                    let device = backend
                        .devices()
                        .into_iter()
                        .find(|device| device.device_id == "living-room")
                        .expect("mock living-room device missing");
                    let _ = Self::device_changed(&emitter, generation, 4, device).await;
                });
            }
            DiscoveryScenario::UnsolicitedEof => {
                let shutdown = self.shared.shutdown_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    shutdown.send_replace(true);
                });
            }
        }
        Ok(generation)
    }

    fn stop_discovery(&self, discovery_generation: u64) -> zbus::fdo::Result<()> {
        let mut discovery = self
            .shared
            .discovery
            .lock()
            .expect("discovery mutex poisoned");
        match discovery.active_generation {
            Some(active) if active == discovery_generation => {
                discovery.active_generation = None;
                Ok(())
            }
            Some(active) => Err(zbus::fdo::Error::InvalidArgs(format!(
                "stale discovery generation {discovery_generation}; active generation is {active}"
            ))),
            None => Err(zbus::fdo::Error::Failed("discovery is not active".into())),
        }
    }

    fn list_devices(&self) -> zbus::fdo::Result<DeviceSnapshot> {
        let (discovery_generation, revision) = {
            let discovery = self
                .shared
                .discovery
                .lock()
                .expect("discovery mutex poisoned");
            let generation = discovery
                .active_generation
                .ok_or_else(|| zbus::fdo::Error::Failed("discovery is not active".into()))?;
            (generation, discovery.revision)
        };
        Ok(DeviceSnapshot {
            discovery_generation,
            revision,
            devices: self.devices(),
        })
    }

    async fn create_session(
        &self,
        session_id: String,
        device_id: String,
        options: SessionOptions,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        options
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        let expected_connection = self.shared.connection_generation.load(Ordering::Acquire);
        if options.connection_generation != expected_connection {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "stale connection generation {}; active generation is {expected_connection}",
                options.connection_generation
            )));
        }
        let discovery_generation = self.active_generation()?;
        if options.discovery_generation != discovery_generation {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "stale discovery generation {}; active generation is {discovery_generation}",
                options.discovery_generation
            )));
        }
        if !self
            .devices()
            .iter()
            .any(|device| device.device_id == device_id)
        {
            return Err(zbus::fdo::Error::InvalidArgs(
                "device is not in the active inventory".into(),
            ));
        }
        let path = session_object_path(&session_id, options.session_generation)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        {
            let mut active = self
                .shared
                .active_session
                .lock()
                .expect("active-session mutex poisoned");
            if active.is_some() {
                return Err(zbus::fdo::Error::Failed(
                    "mock backend supports one active session".into(),
                ));
            }
            *active = Some(session_id.clone());
        }
        let session = match MockSession::new(self.shared.clone(), session_id, device_id, options) {
            Ok(session) => session,
            Err(error) => {
                *self
                    .shared
                    .active_session
                    .lock()
                    .expect("active-session mutex poisoned") = None;
                return Err(media_failure(error));
            }
        };
        if let Err(error) = object_server.at(path.clone(), session).await {
            *self
                .shared
                .active_session
                .lock()
                .expect("active-session mutex poisoned") = None;
            return Err(zbus::fdo::Error::Failed(error.to_string()));
        }
        Ok(path)
    }

    fn shutdown(&self) {
        self.shared.shutdown_tx.send_replace(true);
    }

    #[zbus(signal)]
    async fn device_added(
        emitter: &SignalEmitter<'_>,
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_changed(
        emitter: &SignalEmitter<'_>,
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_removed(
        emitter: &SignalEmitter<'_>,
        discovery_generation: u64,
        revision: u64,
        device: DeviceIdentity,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn fatal_error(
        emitter: &SignalEmitter<'_>,
        connection_generation: u64,
        error_text: String,
    ) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Copy)]
enum DiscoveryScenario {
    Stable,
    RevisionGap,
    UnsolicitedEof,
}

impl DiscoveryScenario {
    fn from_environment() -> anyhow::Result<Self> {
        match std::env::var("PRONK_BACKEND_MOCK_DISCOVERY_SCENARIO")
            .as_deref()
            .unwrap_or("stable")
        {
            "stable" => Ok(Self::Stable),
            "revision-gap" => Ok(Self::RevisionGap),
            "unsolicited-eof" => Ok(Self::UnsolicitedEof),
            value => anyhow::bail!("unknown mock discovery scenario {value:?}"),
        }
    }
}

#[derive(Debug, Clone)]
struct MockSession {
    shared: Arc<MockSessionShared>,
}

#[derive(Debug)]
struct MockSessionShared {
    backend: Arc<MockBackendShared>,
    session_id: String,
    _device_id: String,
    options: SessionOptions,
    state: AsyncMutex<MockSessionState>,
}

#[derive(Debug)]
struct MockSessionState {
    state: SessionState,
    preparation_generation: Option<u64>,
    media_generation: Option<u64>,
    completed_media_generation: Option<u64>,
    media_ready: bool,
    next_control_operation: u64,
    media: MockMediaEngine,
}

impl MockSession {
    fn new(
        backend: Arc<MockBackendShared>,
        session_id: String,
        device_id: String,
        options: SessionOptions,
    ) -> Result<Self, MockMediaError> {
        let media = MockMediaEngine::new(backend.media_mode)?;
        Ok(Self {
            shared: Arc::new(MockSessionShared {
                backend,
                session_id,
                _device_id: device_id,
                options,
                state: AsyncMutex::new(MockSessionState {
                    state: SessionState::Created,
                    preparation_generation: None,
                    media_generation: None,
                    completed_media_generation: None,
                    media_ready: false,
                    next_control_operation: 1,
                    media,
                }),
            }),
        })
    }
}

#[zbus::interface(name = "io.github.pronkproject.Pronk.BackendSession1")]
impl MockSession {
    async fn prepare(&self, request: PreparationRequest) -> zbus::fdo::Result<DeviceCapabilities> {
        request
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        let mut state = self.shared.state.lock().await;
        if state.state != SessionState::Created {
            return Err(zbus::fdo::Error::Failed(
                "Prepare is callable exactly once".into(),
            ));
        }

        let supported_features = SESSION_FEATURE_CONTROL
            | if self.shared.backend.media_mode.supports_audio() {
                SESSION_FEATURE_AUDIO
            } else {
                0
            };
        let features = request.requested_features & supported_features;
        let modes = select_mock_modes(request.candidate_modes);
        let capabilities = DeviceCapabilities {
            preparation_generation: request.preparation_generation,
            display_identity: DisplayIdentity {
                manufacturer_name: Some("Pronk Project".into()),
                manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
                product_name: Some("Deterministic Mock Display".into()),
                product_source: IdentitySource::SetupEndpoint,
                pnp_id: None,
            },
            modes,
            video_profiles: request.video_profiles.into_iter().take(2).collect(),
            audio_profiles: if features & SESSION_FEATURE_AUDIO != 0 {
                request.audio_profiles.into_iter().take(2).collect()
            } else {
                Vec::new()
            },
            features,
        };
        capabilities
            .validate()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        state.state = SessionState::Prepared;
        state.preparation_generation = Some(capabilities.preparation_generation);
        Ok(capabilities)
    }

    async fn configure_media(
        &self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: MediaConfiguration,
        media_generation: u64,
    ) -> zbus::fdo::Result<()> {
        validate_media_configuration(remotes.len(), &targets, &configuration, media_generation)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        let generation = nonzero_media_generation(media_generation)?;
        let mut state = self.shared.state.lock().await;
        if state.state != SessionState::Prepared {
            return Err(invalid_media_transition("ConfigureMedia", state.state));
        }
        if state
            .completed_media_generation
            .is_some_and(|completed| media_generation <= completed)
        {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "media generation {media_generation} is not newer than completed generation {:?}",
                state.completed_media_generation
            )));
        }

        // Admit the generation before crossing the async engine boundary.
        // ConfigureMedia is FD-consuming and its reply is ambiguous if the
        // caller disappears; a matching StopMedia must remain valid either
        // way.
        state.media_generation = Some(media_generation);
        state.media_ready = false;
        state.state = SessionState::Configured;
        let result = state
            .media
            .configure(remotes, targets, &configuration, generation)
            .await;
        if result.is_ok() {
            state.media_ready = true;
        }
        result.map_err(media_failure)
    }

    async fn start(&self, media_generation: u64) -> zbus::fdo::Result<()> {
        let generation = nonzero_media_generation(media_generation)?;
        let mut state = self.shared.state.lock().await;
        require_media_generation(&state, SessionState::Configured, media_generation, "Start")?;
        if !state.media_ready {
            return Err(zbus::fdo::Error::Failed(
                "Start cannot follow a failed media configuration".into(),
            ));
        }
        state.media.start(generation).await.map_err(media_failure)?;
        state.state = SessionState::Streaming;
        Ok(())
    }

    async fn suspend(&self, _reason: SuspendReason) -> zbus::fdo::Result<()> {
        let mut state = self.shared.state.lock().await;
        if state.state != SessionState::Streaming {
            return Err(invalid_media_transition("Suspend", state.state));
        }
        let generation = state
            .media_generation
            .and_then(NonZeroU64::new)
            .ok_or_else(|| zbus::fdo::Error::Failed("media generation is missing".into()))?;
        state
            .media
            .suspend(generation)
            .await
            .map_err(media_failure)?;
        state.state = SessionState::Suspended;
        Ok(())
    }

    async fn resume(&self, media_generation: u64) -> zbus::fdo::Result<()> {
        let generation = nonzero_media_generation(media_generation)?;
        let mut state = self.shared.state.lock().await;
        require_media_generation(&state, SessionState::Suspended, media_generation, "Resume")?;
        state
            .media
            .resume(generation)
            .await
            .map_err(media_failure)?;
        state.state = SessionState::Streaming;
        Ok(())
    }

    async fn stop_media(
        &self,
        media_generation: u64,
        _reason: StopReason,
    ) -> zbus::fdo::Result<()> {
        let generation = nonzero_media_generation(media_generation)?;
        let mut state = self.shared.state.lock().await;
        if state.state == SessionState::Prepared
            && state.completed_media_generation == Some(media_generation)
        {
            return Ok(());
        }
        if !matches!(
            state.state,
            SessionState::Configured | SessionState::Streaming | SessionState::Suspended
        ) {
            return Err(invalid_media_transition("StopMedia", state.state));
        }
        require_matching_media_generation(&state, media_generation, "StopMedia")?;
        let result = state.media.stop(generation).await;
        state.media_generation = None;
        state.completed_media_generation = Some(media_generation);
        state.media_ready = false;
        state.state = SessionState::Prepared;
        result.map_err(media_failure)
    }

    async fn stop(&self, _reason: StopReason) -> zbus::fdo::Result<()> {
        let mut state = self.shared.state.lock().await;
        let result = state.media.shutdown().await;
        state.media_generation = None;
        state.media_ready = false;
        state.state = SessionState::Stopped;
        drop(state);
        let mut active = self
            .shared
            .backend
            .active_session
            .lock()
            .expect("active-session mutex poisoned");
        if active.as_deref() == Some(self.shared.session_id.as_str()) {
            *active = None;
        }
        result.map_err(media_failure)
    }

    async fn transmit_control(
        &self,
        operation: ControlOperation,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<u64> {
        operation
            .validate()
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        if self.shared.options.requested_features & SESSION_FEATURE_CONTROL == 0 {
            return Err(zbus::fdo::Error::NotSupported(
                "control was not requested for this session".into(),
            ));
        }
        if operation.session_generation != self.shared.options.session_generation {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "control session generation {} differs from {}",
                operation.session_generation, self.shared.options.session_generation
            )));
        }
        let operation_id = {
            let mut state = self.shared.state.lock().await;
            if matches!(
                state.state,
                SessionState::Created | SessionState::Stopped | SessionState::Failed
            ) {
                return Err(zbus::fdo::Error::Failed(format!(
                    "TransmitControl is invalid from {:?}",
                    state.state
                )));
            }
            let operation_id = state.next_control_operation;
            state.next_control_operation =
                state.next_control_operation.checked_add(1).ok_or_else(|| {
                    zbus::fdo::Error::Failed("control operation IDs exhausted".into())
                })?;
            operation_id
        };
        Self::control_completed(
            &emitter,
            self.shared.options.session_generation,
            operation_id,
            true,
            String::new(),
        )
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok(operation_id)
    }

    async fn get_statistics(&self) -> zbus::fdo::Result<SessionStatistics> {
        let state = self.shared.state.lock().await;
        if !state.media_ready
            || !matches!(
                state.state,
                SessionState::Configured | SessionState::Streaming | SessionState::Suspended
            )
        {
            return Err(zbus::fdo::Error::NotSupported(
                "media is not configured".into(),
            ));
        }
        let generation = state
            .media_generation
            .and_then(NonZeroU64::new)
            .ok_or_else(|| zbus::fdo::Error::Failed("media generation is missing".into()))?;
        let statistics = state
            .media
            .statistics(self.shared.options.session_generation, generation)
            .await
            .map_err(media_failure)?;
        statistics
            .validate()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok(statistics)
    }

    #[zbus(signal)]
    async fn state_changed(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        media_generation: u64,
        state: SessionState,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn disconnected(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        error_text: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn keyframe_requested(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        media_generation: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn bitrate_requested(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        media_generation: u64,
        bitrate: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn control_completed(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        operation_id: u64,
        succeeded: bool,
        error_text: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn fatal_error(
        emitter: &SignalEmitter<'_>,
        session_generation: u64,
        error_text: String,
    ) -> zbus::Result<()>;
}

fn select_mock_modes(candidate_modes: Vec<DisplayMode>) -> Vec<DisplayMode> {
    let required_compatibility_mode = candidate_modes
        .iter()
        .find(|mode| {
            mode.width == 640
                && mode.height == 480
                && mode.refresh_millihz == 60_000
                && mode.flags == 0
        })
        .cloned();
    let mut modes: Vec<_> = candidate_modes.into_iter().take(MAX_MOCK_MODES).collect();
    if let Some(required) = required_compatibility_mode {
        if !modes.contains(&required) {
            if let Some(last) = modes.last_mut() {
                *last = required;
            }
        }
    }
    modes
}

fn require_media_generation(
    state: &MockSessionState,
    required_state: SessionState,
    media_generation: u64,
    operation: &'static str,
) -> zbus::fdo::Result<()> {
    if state.state != required_state {
        return Err(invalid_media_transition(operation, state.state));
    }
    require_matching_media_generation(state, media_generation, operation)
}

fn require_matching_media_generation(
    state: &MockSessionState,
    media_generation: u64,
    operation: &'static str,
) -> zbus::fdo::Result<()> {
    if state.media_generation != Some(media_generation) {
        return Err(zbus::fdo::Error::Failed(format!(
            "{operation} media generation {media_generation} does not match active generation {:?}",
            state.media_generation
        )));
    }
    Ok(())
}

fn invalid_media_transition(operation: &'static str, state: SessionState) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(format!(
        "{operation} is invalid while media session is {state:?}"
    ))
}

fn nonzero_media_generation(value: u64) -> zbus::fdo::Result<NonZeroU64> {
    NonZeroU64::new(value)
        .ok_or_else(|| zbus::fdo::Error::InvalidArgs("media generation must be nonzero".into()))
}

fn media_failure(error: MockMediaError) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}

fn backend_info_from_environment() -> anyhow::Result<BackendInfo> {
    let mut info = BackendInfo::v1(
        MOCK_BACKEND_ID,
        "Deterministic mock backend",
        env!("CARGO_PKG_VERSION"),
        environment_or("PRONK_BACKEND_INSTANCE", "mock"),
        environment_or("INVOCATION_ID", "development"),
    );
    if let Some(value) = std::env::var_os("PRONK_BACKEND_MOCK_PROTOCOL_MAJOR") {
        info.protocol_major = value
            .to_str()
            .context("PRONK_BACKEND_MOCK_PROTOCOL_MAJOR is not UTF-8")?
            .parse()
            .context("parse PRONK_BACKEND_MOCK_PROTOCOL_MAJOR")?;
    }
    // Deliberately do not validate here: the stale-major gate must prove that
    // the host rejects an incompatible peer during RegisterBackend().
    ensure!(info.protocol_major > 0, "protocol major must be nonzero");
    Ok(info)
}

fn environment_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.into())
}

fn scrub_ambient_pipewire_environment() {
    for name in ["PIPEWIRE_REMOTE", "PIPEWIRE_RUNTIME_DIR"] {
        std::env::remove_var(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32) -> DisplayMode {
        DisplayMode {
            width,
            height,
            refresh_millihz: 60_000,
            flags: 0,
        }
    }

    #[test]
    fn mock_modes_preserve_the_standard_offer_and_bound_compatibility_repair() {
        let standard = vec![mode(1920, 1080), mode(1280, 720), mode(640, 480)];
        assert_eq!(select_mock_modes(standard.clone()), standard);

        let repaired = select_mock_modes(vec![
            mode(3840, 2160),
            mode(1920, 1080),
            mode(1280, 720),
            mode(640, 480),
        ]);
        assert_eq!(
            repaired,
            vec![mode(3840, 2160), mode(1920, 1080), mode(640, 480)]
        );
    }
}
