use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use chromiacast::{CastDevice, CastEndpoint};
use pronk_backend_protocol::{
    DeviceAvailability, DeviceIdentity, DeviceInfo, DeviceSnapshot, DiscoveryMetadataEntry,
    Validate, MAX_DEVICES, MAX_ENDPOINTS,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub const CHROMIACAST_BACKEND_ID: &str = "chromiacast";
pub const FIXTURE_DEVICE_ID: &str = "00112233445566778899aabbccddeeff";

const COMMAND_QUEUE_CAPACITY: usize = 8;
const EVENT_QUEUE_CAPACITY: usize = MAX_DEVICES * 2;
const DEFAULT_SCAN_WINDOW: Duration = Duration::from_secs(2);
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_SCAN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub info: DeviceInfo,
    pub endpoints: Vec<SocketAddr>,
}

impl DeviceRecord {
    fn identity(&self) -> DeviceIdentity {
        DeviceIdentity {
            backend_id: self.info.backend_id.clone(),
            device_id: self.info.device_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Added {
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    },
    Changed {
        discovery_generation: u64,
        revision: u64,
        device: DeviceInfo,
    },
    Removed {
        discovery_generation: u64,
        revision: u64,
        device: DeviceIdentity,
    },
    Fatal {
        error_text: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct DiscoveryConfiguration {
    scan_window: Duration,
    scan_interval: Duration,
    scan_timeout: Duration,
}

impl Default for DiscoveryConfiguration {
    fn default() -> Self {
        Self {
            scan_window: DEFAULT_SCAN_WINDOW,
            scan_interval: DEFAULT_SCAN_INTERVAL,
            scan_timeout: DEFAULT_SCAN_TIMEOUT,
        }
    }
}

impl DiscoveryConfiguration {
    #[cfg(test)]
    fn for_test(scan_interval: Duration) -> Self {
        Self {
            scan_window: Duration::from_millis(10),
            scan_interval,
            scan_timeout: Duration::from_millis(100),
        }
    }

    #[cfg(test)]
    fn for_stalled_source_test() -> Self {
        Self {
            scan_window: Duration::from_millis(1),
            scan_interval: Duration::from_secs(60),
            scan_timeout: Duration::from_millis(10),
        }
    }
}

#[async_trait]
pub trait DiscoverySource: Send + 'static {
    async fn scan(&mut self, window: Duration) -> Result<Vec<DeviceRecord>, DiscoverySourceError>;
}

#[derive(Debug, Default)]
pub struct ChromiacastDiscoverySource;

#[async_trait]
impl DiscoverySource for ChromiacastDiscoverySource {
    async fn scan(&mut self, window: Duration) -> Result<Vec<DeviceRecord>, DiscoverySourceError> {
        let devices = chromiacast::discover(window)
            .await
            .map_err(|error| DiscoverySourceError::Scan(error.to_string()))?;
        Ok(devices
            .into_iter()
            .filter_map(record_from_cast_device)
            .collect())
    }
}

#[derive(Debug, Default)]
pub struct EmptyTestDiscoverySource;

#[async_trait]
impl DiscoverySource for EmptyTestDiscoverySource {
    async fn scan(&mut self, _window: Duration) -> Result<Vec<DeviceRecord>, DiscoverySourceError> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Default)]
pub struct FixtureTestDiscoverySource;

#[async_trait]
impl DiscoverySource for FixtureTestDiscoverySource {
    async fn scan(&mut self, _window: Duration) -> Result<Vec<DeviceRecord>, DiscoverySourceError> {
        Ok(vec![DeviceRecord {
            info: DeviceInfo {
                backend_id: CHROMIACAST_BACKEND_ID.into(),
                device_id: FIXTURE_DEVICE_ID.into(),
                display_name: "Fixture Living Room TV".into(),
                availability: DeviceAvailability::Available,
                metadata: vec![
                    DiscoveryMetadataEntry {
                        key: "device-kind".into(),
                        value: "google-cast".into(),
                    },
                    DiscoveryMetadataEntry {
                        key: "cast-capabilities".into(),
                        value: "5".into(),
                    },
                    DiscoveryMetadataEntry {
                        key: "model".into(),
                        value: "Discovery Model Must Not Become Identity".into(),
                    },
                ],
            },
            endpoints: vec!["192.0.2.1:8009"
                .parse()
                .expect("fixed fixture endpoint is valid")],
        }])
    }
}

fn record_from_cast_device(device: CastDevice) -> Option<DeviceRecord> {
    let mut metadata = vec![
        DiscoveryMetadataEntry {
            key: "device-kind".into(),
            value: "google-cast".into(),
        },
        DiscoveryMetadataEntry {
            key: "cast-capabilities".into(),
            value: device.capabilities().bits().to_string(),
        },
    ];
    if !device.model().is_empty() {
        metadata.push(DiscoveryMetadataEntry {
            key: "model".into(),
            value: device.model().into(),
        });
    }
    if let Some(version) = device.protocol_version() {
        metadata.push(DiscoveryMetadataEntry {
            key: "cast-protocol-version".into(),
            value: version.to_string(),
        });
    }
    let info = DeviceInfo {
        backend_id: CHROMIACAST_BACKEND_ID.into(),
        device_id: device.id().into(),
        display_name: device.name().into(),
        availability: DeviceAvailability::Available,
        metadata,
    };
    if info.validate().is_err() {
        return None;
    }
    let endpoints =
        distinct_endpoint_addresses(device.endpoints().iter().map(CastEndpoint::address));
    Some(DeviceRecord { info, endpoints })
}

fn distinct_endpoint_addresses(endpoints: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    endpoints
        .into_iter()
        .filter(|endpoint| seen.insert(*endpoint))
        .collect()
}

#[derive(Debug, Error)]
pub enum DiscoverySourceError {
    #[error("Chromiacast discovery failed: {0}")]
    Scan(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DiscoveryActorError {
    #[error("discovery actor has stopped")]
    Stopped,
    #[error("discovery is not active")]
    NotActive,
    #[error("stale discovery generation {actual}; active generation is {expected}")]
    StaleGeneration { expected: u64, actual: u64 },
    #[error("discovery generation counter is exhausted")]
    GenerationExhausted,
    #[error("discovery revision counter is exhausted")]
    RevisionExhausted,
    #[error("discovery snapshot contains more than {MAX_DEVICES} devices")]
    TooManyDevices,
    #[error("discovery snapshot repeats device ID {0:?}")]
    DuplicateDevice(String),
    #[error("discovery source produced an invalid device: {0}")]
    InvalidDevice(String),
    #[error("discovery source failed: {0}")]
    Source(String),
}

#[derive(Debug, Clone)]
pub struct DiscoveryHandle {
    commands: mpsc::Sender<DiscoveryCommand>,
}

impl DiscoveryHandle {
    pub async fn start(&self) -> Result<u64, DiscoveryActorError> {
        self.request(|reply| DiscoveryCommand::Start { reply })
            .await
    }

    pub async fn stop(&self, generation: u64) -> Result<(), DiscoveryActorError> {
        self.request(|reply| DiscoveryCommand::Stop { generation, reply })
            .await
    }

    pub async fn snapshot(&self) -> Result<DeviceSnapshot, DiscoveryActorError> {
        self.request(|reply| DiscoveryCommand::Snapshot { reply })
            .await
    }

    pub async fn resolve(
        &self,
        generation: u64,
        device_id: String,
    ) -> Result<Option<DeviceRecord>, DiscoveryActorError> {
        self.request(|reply| DiscoveryCommand::Resolve {
            generation,
            device_id,
            reply,
        })
        .await
    }

    async fn shutdown(&self) -> Result<(), DiscoveryActorError> {
        self.request(|reply| DiscoveryCommand::Shutdown { reply })
            .await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, DiscoveryActorError>>) -> DiscoveryCommand,
    ) -> Result<T, DiscoveryActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(command(reply_tx))
            .await
            .map_err(|_| DiscoveryActorError::Stopped)?;
        reply_rx.await.map_err(|_| DiscoveryActorError::Stopped)?
    }
}

#[derive(Debug)]
pub struct DiscoveryActor {
    handle: DiscoveryHandle,
    task: Option<JoinHandle<()>>,
}

impl DiscoveryActor {
    pub fn spawn(
        source: Box<dyn DiscoverySource>,
        configuration: DiscoveryConfiguration,
    ) -> (Self, DiscoveryHandle, mpsc::Receiver<DiscoveryEvent>) {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let handle = DiscoveryHandle {
            commands: command_tx,
        };
        let task = tokio::spawn(run_actor(source, configuration, command_rx, event_tx));
        (
            Self {
                handle: handle.clone(),
                task: Some(task),
            },
            handle,
            event_rx,
        )
    }

    pub async fn shutdown(mut self) -> Result<(), DiscoveryActorError> {
        let response = self.handle.shutdown().await;
        if let Some(task) = self.task.take() {
            task.await.map_err(|_| DiscoveryActorError::Stopped)?;
        }
        match response {
            Err(DiscoveryActorError::Stopped) => Ok(()),
            response => response,
        }
    }
}

impl Drop for DiscoveryActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug)]
enum DiscoveryCommand {
    Start {
        reply: oneshot::Sender<Result<u64, DiscoveryActorError>>,
    },
    Stop {
        generation: u64,
        reply: oneshot::Sender<Result<(), DiscoveryActorError>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<DeviceSnapshot, DiscoveryActorError>>,
    },
    Resolve {
        generation: u64,
        device_id: String,
        reply: oneshot::Sender<Result<Option<DeviceRecord>, DiscoveryActorError>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), DiscoveryActorError>>,
    },
}

#[derive(Debug)]
struct ActiveDiscovery {
    generation: u64,
    revision: u64,
    devices: BTreeMap<String, DeviceRecord>,
    next_scan: Instant,
}

#[derive(Debug, Default)]
struct ActorState {
    next_generation: u64,
    active: Option<ActiveDiscovery>,
}

async fn run_actor(
    mut source: Box<dyn DiscoverySource>,
    configuration: DiscoveryConfiguration,
    mut commands: mpsc::Receiver<DiscoveryCommand>,
    events: mpsc::Sender<DiscoveryEvent>,
) {
    let mut state = ActorState::default();
    loop {
        let command = if let Some(active) = &state.active {
            tokio::select! {
                command = commands.recv() => command,
                _ = tokio::time::sleep_until(active.next_scan) => {
                    let result = match tokio::time::timeout(
                        configuration.scan_timeout,
                        source.scan(configuration.scan_window),
                    ).await {
                        Ok(Ok(devices)) => apply_scan(&mut state, devices, &events).await,
                        Ok(Err(error)) => Err(DiscoveryActorError::Source(error.to_string())),
                        Err(_) => Err(DiscoveryActorError::Source(format!(
                            "scan exceeded {:?}",
                            configuration.scan_timeout,
                        ))),
                    };
                    if let Err(error) = result {
                        let _ = events.send(DiscoveryEvent::Fatal {
                            error_text: error.to_string(),
                        }).await;
                        break;
                    }
                    if let Some(active) = &mut state.active {
                        active.next_scan = Instant::now() + configuration.scan_interval;
                    }
                    continue;
                }
            }
        } else {
            commands.recv().await
        };
        let Some(command) = command else {
            break;
        };
        match command {
            DiscoveryCommand::Start { reply } => {
                let result = start_discovery(&mut state);
                let _ = reply.send(result);
            }
            DiscoveryCommand::Stop { generation, reply } => {
                let result = stop_discovery(&mut state, generation);
                let _ = reply.send(result);
            }
            DiscoveryCommand::Snapshot { reply } => {
                let _ = reply.send(snapshot(&state));
            }
            DiscoveryCommand::Resolve {
                generation,
                device_id,
                reply,
            } => {
                let _ = reply.send(resolve(&state, generation, &device_id));
            }
            DiscoveryCommand::Shutdown { reply } => {
                state.active = None;
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn start_discovery(state: &mut ActorState) -> Result<u64, DiscoveryActorError> {
    if let Some(active) = &state.active {
        return Ok(active.generation);
    }
    state.next_generation = state
        .next_generation
        .checked_add(1)
        .ok_or(DiscoveryActorError::GenerationExhausted)?;
    state.active = Some(ActiveDiscovery {
        generation: state.next_generation,
        // Revision zero is reserved for "no inventory" by the coordinator.
        // The empty baseline returned before the first scan is still a real
        // inventory snapshot, so begin it at revision one.
        revision: 1,
        devices: BTreeMap::new(),
        next_scan: Instant::now(),
    });
    Ok(state.next_generation)
}

fn stop_discovery(state: &mut ActorState, generation: u64) -> Result<(), DiscoveryActorError> {
    let active = state
        .active
        .as_ref()
        .ok_or(DiscoveryActorError::NotActive)?;
    if active.generation != generation {
        return Err(DiscoveryActorError::StaleGeneration {
            expected: active.generation,
            actual: generation,
        });
    }
    state.active = None;
    Ok(())
}

fn snapshot(state: &ActorState) -> Result<DeviceSnapshot, DiscoveryActorError> {
    let active = state
        .active
        .as_ref()
        .ok_or(DiscoveryActorError::NotActive)?;
    Ok(DeviceSnapshot {
        discovery_generation: active.generation,
        revision: active.revision,
        devices: active
            .devices
            .values()
            .map(|record| record.info.clone())
            .collect(),
    })
}

fn resolve(
    state: &ActorState,
    generation: u64,
    device_id: &str,
) -> Result<Option<DeviceRecord>, DiscoveryActorError> {
    let active = state
        .active
        .as_ref()
        .ok_or(DiscoveryActorError::NotActive)?;
    if active.generation != generation {
        return Err(DiscoveryActorError::StaleGeneration {
            expected: active.generation,
            actual: generation,
        });
    }
    Ok(active.devices.get(device_id).cloned())
}

async fn apply_scan(
    state: &mut ActorState,
    devices: Vec<DeviceRecord>,
    events: &mpsc::Sender<DiscoveryEvent>,
) -> Result<(), DiscoveryActorError> {
    if devices.len() > MAX_DEVICES {
        return Err(DiscoveryActorError::TooManyDevices);
    }
    let mut next = BTreeMap::new();
    for record in devices {
        record
            .info
            .validate()
            .map_err(|error| DiscoveryActorError::InvalidDevice(error.to_string()))?;
        if record.info.backend_id != CHROMIACAST_BACKEND_ID
            || record.endpoints.is_empty()
            || record.endpoints.len() > MAX_ENDPOINTS
            || record.endpoints.iter().any(|endpoint| {
                endpoint.port() == 0
                    || endpoint.ip().is_unspecified()
                    || endpoint.ip().is_multicast()
            })
        {
            return Err(DiscoveryActorError::InvalidDevice(
                "wrong backend ID or invalid endpoint set".into(),
            ));
        }
        let mut unique_endpoints = record.endpoints.clone();
        unique_endpoints.sort_unstable();
        unique_endpoints.dedup();
        if unique_endpoints.len() != record.endpoints.len() {
            return Err(DiscoveryActorError::InvalidDevice(
                "duplicate device endpoint".into(),
            ));
        }
        let id = record.info.device_id.clone();
        if next.insert(id.clone(), record).is_some() {
            return Err(DiscoveryActorError::DuplicateDevice(id));
        }
    }

    let active = state
        .active
        .as_mut()
        .ok_or(DiscoveryActorError::NotActive)?;
    let generation = active.generation;
    for removed_id in active
        .devices
        .keys()
        .filter(|id| !next.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>()
    {
        active.revision = next_revision(active.revision)?;
        let removed = active
            .devices
            .get(&removed_id)
            .expect("removed key came from current inventory")
            .identity();
        events
            .send(DiscoveryEvent::Removed {
                discovery_generation: generation,
                revision: active.revision,
                device: removed,
            })
            .await
            .map_err(|_| DiscoveryActorError::Stopped)?;
    }
    for (id, record) in &next {
        let event = match active.devices.get(id) {
            None => Some(DiscoveryEvent::Added {
                discovery_generation: generation,
                revision: next_revision(active.revision)?,
                device: record.info.clone(),
            }),
            Some(current) if current != record => Some(DiscoveryEvent::Changed {
                discovery_generation: generation,
                revision: next_revision(active.revision)?,
                device: record.info.clone(),
            }),
            Some(_) => None,
        };
        if let Some(event) = event {
            active.revision = event_revision(&event);
            events
                .send(event)
                .await
                .map_err(|_| DiscoveryActorError::Stopped)?;
        }
    }
    active.devices = next;
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, DiscoveryActorError> {
    revision
        .checked_add(1)
        .ok_or(DiscoveryActorError::RevisionExhausted)
}

fn event_revision(event: &DiscoveryEvent) -> u64 {
    match event {
        DiscoveryEvent::Added { revision, .. }
        | DiscoveryEvent::Changed { revision, .. }
        | DiscoveryEvent::Removed { revision, .. } => *revision,
        DiscoveryEvent::Fatal { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::*;

    struct RecordingSource {
        calls: Arc<AtomicUsize>,
        called: Arc<Notify>,
    }

    struct StalledSource;

    #[async_trait]
    impl DiscoverySource for RecordingSource {
        async fn scan(
            &mut self,
            _window: Duration,
        ) -> Result<Vec<DeviceRecord>, DiscoverySourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.called.notify_one();
            Ok(vec![record("living-room", "Living Room")])
        }
    }

    #[async_trait]
    impl DiscoverySource for StalledSource {
        async fn scan(
            &mut self,
            _window: Duration,
        ) -> Result<Vec<DeviceRecord>, DiscoverySourceError> {
            std::future::pending().await
        }
    }

    fn record(id: &str, name: &str) -> DeviceRecord {
        DeviceRecord {
            info: DeviceInfo {
                backend_id: CHROMIACAST_BACKEND_ID.into(),
                device_id: id.into(),
                display_name: name.into(),
                availability: DeviceAvailability::Available,
                metadata: Vec::new(),
            },
            endpoints: vec!["192.0.2.1:8009".parse().unwrap()],
        }
    }

    #[test]
    fn projection_coalesces_interface_distinct_routes_with_the_same_address() {
        let endpoints = distinct_endpoint_addresses([
            "192.0.2.1:8009".parse().unwrap(),
            "192.0.2.1:8009".parse().unwrap(),
            "[2001:db8::1]:8009".parse().unwrap(),
        ]);
        assert_eq!(
            endpoints,
            [
                "192.0.2.1:8009".parse().unwrap(),
                "[2001:db8::1]:8009".parse().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn source_is_idle_until_start_and_generation_checked_afterward() {
        let calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::new(Notify::new());
        let (actor, handle, mut events) = DiscoveryActor::spawn(
            Box::new(RecordingSource {
                calls: Arc::clone(&calls),
                called: Arc::clone(&called),
            }),
            DiscoveryConfiguration::for_test(Duration::from_secs(60)),
        );
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let generation = handle.start().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), called.notified())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            events.recv().await,
            Some(DiscoveryEvent::Added {
                discovery_generation: 1,
                revision: 2,
                ..
            })
        ));
        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.devices[0].device_id, "living-room");
        assert_eq!(
            handle.stop(generation + 1).await,
            Err(DiscoveryActorError::StaleGeneration {
                expected: generation,
                actual: generation + 1,
            })
        );
        handle.stop(generation).await.unwrap();
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stalled_scan_fails_closed_within_the_actor_deadline() {
        let (actor, handle, mut events) = DiscoveryActor::spawn(
            Box::new(StalledSource),
            DiscoveryConfiguration::for_stalled_source_test(),
        );
        handle.start().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("stalled scan must reach its deadline")
            .expect("stalled scan must emit a terminal event");
        assert!(matches!(event, DiscoveryEvent::Fatal { .. }));
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scan_diff_emits_contiguous_device_events() {
        let mut state = ActorState::default();
        let generation = start_discovery(&mut state).unwrap();
        let (events_tx, mut events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        apply_scan(
            &mut state,
            vec![record("one", "One"), record("two", "Two")],
            &events_tx,
        )
        .await
        .unwrap();
        apply_scan(
            &mut state,
            vec![record("two", "Two renamed"), record("three", "Three")],
            &events_tx,
        )
        .await
        .unwrap();

        let revisions = [
            events_rx.recv().await.unwrap(),
            events_rx.recv().await.unwrap(),
            events_rx.recv().await.unwrap(),
            events_rx.recv().await.unwrap(),
            events_rx.recv().await.unwrap(),
        ]
        .map(|event| event_revision(&event));
        assert_eq!(revisions, [2, 3, 4, 5, 6]);
        let snapshot = snapshot(&state).unwrap();
        assert_eq!(snapshot.discovery_generation, generation);
        assert_eq!(snapshot.revision, 6);
        assert_eq!(snapshot.devices.len(), 2);
    }
}
