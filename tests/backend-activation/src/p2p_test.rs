use std::fs;
use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pronk::device_session::BackendDeviceSession;
use pronk::device_session_port::{
    DeviceMediaConfiguration, DeviceMediaEndpoint, DeviceMediaKind, DeviceMediaSetup,
    DeviceMediaStopReason, DeviceMediaSuspendReason, DeviceMediaTarget, DeviceSessionPort,
    DeviceSessionStopReason,
};
use pronk::display::DisplaySetupStage;
use pronk::display_state::RoutedMode;
use pronk::manager::{
    BackendConfig, InventoryEvent, ManagerActor, OutputInventoryProvider,
    OutputInventoryProviderError,
};
use pronk::mutter_grant_provider::MutterGrantProvider;
use pronk::preparation::PreparedCastDevice;
use pronk_backend_host::{
    BackendConnectError, BackendConnection, BackendDisconnectReason, BackendEndpoint,
    BackendReconnectPolicy, BackendRetryError, BackendSessionError, BackendSessionRequest,
    BackendSupervisor, BackendSupervisorEvent, DiscoveryNotification, ExactRegistrationValidator,
};
use pronk_backend_protocol::{
    AudioProfile, Backend1Proxy, BackendSession1Proxy, ControlKind, ControlOperation,
    DeviceAvailability, DisplayMode, MediaConfiguration, MediaKind, PipeWireTarget,
    PreparationRequest, SessionOptions, StopReason, SuspendReason, Validate, VideoProfile,
    SESSION_FEATURE_AUDIO, SESSION_FEATURE_CONTROL,
};
use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID, SYSTEM_PNP_IDS_PATH};
use pronk_core::output::{
    discover_castkms_outputs, CastKmsOutput, CastKmsOutputId, OutputConnection,
};
use pronk_core::session::PinnedCallerSession;
use pronk_dbus::DeviceSelection;
use pronk_pipewire::{ClassifiedSocketPaths, ClassifiedSocketRemoteProvider};
use pronk_systemd::BACKEND_CONTROL_FD_NAME;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

mod gstreamer_fixture;
mod test_grant_provider;

use gstreamer_fixture::{
    pass_gstreamer_diagnostic_environment, wait_for_backend_pipewire_clients, GStreamerTestProducer,
};
use test_grant_provider::UnreachableGrantProvider;

const START_TIMEOUT: Duration = Duration::from_secs(5);
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_GENERATION_ONE: u64 = 41;
const CONNECTION_GENERATION_TWO: u64 = 42;

#[derive(Debug)]
struct StaticOutputInventoryProvider {
    outputs: Vec<CastKmsOutput>,
}

impl OutputInventoryProvider for StaticOutputInventoryProvider {
    fn discover(&self) -> Result<Vec<CastKmsOutput>, OutputInventoryProviderError> {
        Ok(self.outputs.clone())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let (socket_activate, mock_backend) = parse_arguments()?;
    let media_gate = match std::env::var_os("PRONK_BACKEND_MEDIA_GATE") {
        None => false,
        Some(value) if value == "real" => true,
        Some(_) => bail!("PRONK_BACKEND_MEDIA_GATE must be unset or 'real'"),
    };
    if media_gate {
        let path = temporary_socket_path("gstreamer-media");
        let mut backend =
            ActivationLauncher::start_gstreamer(&socket_activate, &mock_backend, &path)?;
        backend.wait_until_listening().await?;
        run_gstreamer_media_gate(&path, &mut backend).await?;
        backend.stop()?;
        println!("p2p_passed_fd_no_ambient_fallback=pass");
        println!("p2p_exact_pipewire_target=pass");
        println!("p2p_h264_annex_b_encoder=pass");
        println!("p2p_media_suspend_resume=pass");
        println!("p2p_media_statistics=pass");
        println!("p2p_source_loss_no_reconnect=pass");
        println!("p2p_backend_crash_fd_cleanup=pass");
        println!("p2p_backend_media_reactivation=pass");
        println!("p2p_fresh_remote_replacement=pass");
        return Ok(());
    }
    let real_display_setup = match std::env::var_os("PRONK_VM_DISPLAY_SETUP_GATE") {
        None => false,
        Some(value) if value == "real" => true,
        Some(_) => bail!("PRONK_VM_DISPLAY_SETUP_GATE must be unset or 'real'"),
    };

    let normal_path = temporary_socket_path("normal");
    let mut normal =
        ActivationLauncher::start(&socket_activate, &mock_backend, &normal_path, None, None)?;
    normal.wait_until_listening().await?;
    run_valid_connection(&normal_path, CONNECTION_GENERATION_ONE).await?;
    run_valid_connection(&normal_path, CONNECTION_GENERATION_TWO).await?;
    normal.stop()?;

    let stale_path = temporary_socket_path("stale-major");
    let mut stale = ActivationLauncher::start(
        &socket_activate,
        &mock_backend,
        &stale_path,
        Some("2"),
        None,
    )?;
    stale.wait_until_listening().await?;
    run_stale_major_connection(&stale_path).await?;
    stale.stop()?;

    let gap_path = temporary_socket_path("revision-gap");
    let mut gap = ActivationLauncher::start(
        &socket_activate,
        &mock_backend,
        &gap_path,
        None,
        Some("revision-gap"),
    )?;
    gap.wait_until_listening().await?;
    run_revision_gap_connection(&gap_path).await?;
    gap.stop()?;

    let manager_path = temporary_socket_path("inventory-manager");
    let mut manager =
        ActivationLauncher::start(&socket_activate, &mock_backend, &manager_path, None, None)?;
    manager.wait_until_listening().await?;
    run_inventory_manager(&manager_path).await?;
    run_supervised_preparation(&manager_path).await?;
    if real_display_setup {
        run_real_display_setup(&manager_path).await?;
    }
    manager.stop()?;

    let eof_path = temporary_socket_path("unsolicited-eof");
    let mut eof = ActivationLauncher::start(
        &socket_activate,
        &mock_backend,
        &eof_path,
        None,
        Some("unsolicited-eof"),
    )?;
    eof.wait_until_listening().await?;
    run_unsolicited_eof_connection(&eof_path).await?;
    run_supervised_reactivation(&eof_path).await?;
    eof.stop()?;

    println!("p2p_external_same_uid=pass");
    println!("p2p_host_connection_owner=pass");
    println!("p2p_register_exactly_once=pass");
    println!("p2p_revisioned_snapshot=pass");
    println!("p2p_list_signal_race=pass");
    println!("p2p_stale_generation=pass");
    println!("p2p_revision_gap_resnapshot=pass");
    println!("p2p_unsolicited_eof=pass");
    println!("backend_supervisor_unavailable=pass");
    println!("backend_supervisor_reactivation=pass");
    println!("backend_supervisor_shutdown=pass");
    println!("device_inventory_manager=pass");
    println!("device_inventory_discovery_only=pass");
    println!("display_slot_reservation=pass");
    println!("display_slot_drop_release=pass");
    println!("backend_supervisor_session_prepare=pass");
    println!("p2p_control_completion=pass");
    println!("p2p_prepare_without_media_fd=pass");
    println!("p2p_media_fd_lifecycle=pass");
    println!("p2p_shutdown_eof=pass");
    println!("systemd_socket_reactivation=pass");
    println!("p2p_incompatible_major=pass");
    if real_display_setup {
        println!("caller_to_mutter_grant=pass");
        println!("selected_device_atomic_attach=pass");
        println!("added_display_remove_cleanup=pass");
    }
    Ok(())
}

async fn run_gstreamer_media_gate(
    path: &Path,
    launcher: &mut ActivationLauncher,
) -> anyhow::Result<()> {
    let producer = GStreamerTestProducer::start_with_format("selected", 320, 240, 30).await?;
    let unrelated = GStreamerTestProducer::start_with_format("unrelated", 320, 240, 30).await?;
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    let policy = BackendReconnectPolicy::new(
        1,
        Duration::from_millis(10),
        Duration::from_millis(10),
        Duration::from_secs(1),
    )?;
    let mut supervisor = BackendSupervisor::spawn(
        endpoint,
        171,
        Arc::new(ExactRegistrationValidator::new("mock", "development")),
        policy,
    )?;
    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::Connecting {
                connection_generation: 171,
            },
        "media-gate supervisor did not begin connecting"
    );
    let discovery_generation = match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Connected {
            connection_generation,
            inventory,
            ..
        } => {
            ensure!(
                connection_generation == 171,
                "wrong media-gate connection generation"
            );
            inventory.discovery_generation
        }
        event => bail!("unexpected media-gate supervisor event: {event:?}"),
    };

    let session_id = "71234567-89ab-cdef-0123-456789abcdef";
    let request = BackendSessionRequest::new(
        session_id,
        "living-room",
        SessionOptions {
            connection_generation: 171,
            discovery_generation,
            session_generation: 1,
            requested_features: 0,
        },
    )?;
    let session = supervisor.handle().create_session(request).await?;
    session.prepare(media_gate_preparation_request()).await?;

    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("media gate requires XDG_RUNTIME_DIR")?;
    let remote_provider = ClassifiedSocketRemoteProvider::new(
        ClassifiedSocketPaths::in_runtime_dir(runtime_dir)
            .context("construct media-gate classified PipeWire paths")?,
    );

    // A disconnected non-PipeWire fd must fail rather than opening the
    // poisoned ambient remote inherited by the activation wrapper.
    let invalid_fd_generation = NonZeroU64::new(1).unwrap();
    let (invalid_remote, invalid_peer) =
        StdUnixStream::pair().context("create disconnected non-PipeWire media remote")?;
    drop(invalid_peer);
    let invalid_remote: zbus::zvariant::OwnedFd = std::os::fd::OwnedFd::from(invalid_remote).into();
    if session
        .configure_media(
            vec![invalid_remote],
            vec![media_gate_target(
                &producer.node_name,
                producer.object_serial.get(),
                session_id,
                invalid_fd_generation,
            )],
            media_gate_configuration(),
            invalid_fd_generation,
        )
        .await
        .is_ok()
    {
        ensure!(
            session.start_media(invalid_fd_generation).await.is_err(),
            "backend ignored a passed non-PipeWire fd and opened an ambient remote"
        );
    }
    session
        .stop_media(invalid_fd_generation, StopReason::TransportFailure)
        .await
        .context("clean up rejected non-PipeWire media remote")?;

    // A valid connection paired with a nonexistent exact target must not
    // silently select another node on the same server (or any ambient
    // PipeWire remote). Configure may fail immediately or Start may fail
    // while waiting for a frame; both are correct fail-closed outcomes.
    let rejected_generation = NonZeroU64::new(2).unwrap();
    let rejected_remote =
        media_gate_remote(&remote_provider, session_id, rejected_generation).await?;
    let nonexistent_name = format!("{}.missing", producer.node_name);
    let rejected_target = media_gate_target(
        &nonexistent_name,
        producer.object_serial.get(),
        session_id,
        rejected_generation,
    );
    if session
        .configure_media(
            vec![rejected_remote],
            vec![rejected_target],
            media_gate_configuration(),
            rejected_generation,
        )
        .await
        .is_ok()
    {
        ensure!(
            session.start_media(rejected_generation).await.is_err(),
            "backend streamed from a fallback node for a nonexistent exact target"
        );
    }
    session
        .stop_media(rejected_generation, StopReason::TransportFailure)
        .await
        .context("clean up rejected exact PipeWire target")?;

    let media_generation = NonZeroU64::new(3).unwrap();
    let remote = media_gate_remote(&remote_provider, session_id, media_generation).await?;
    session
        .configure_media(
            vec![remote],
            vec![media_gate_target(
                &producer.node_name,
                producer.object_serial.get(),
                session_id,
                media_generation,
            )],
            media_gate_configuration(),
            media_generation,
        )
        .await
        .context("configure exact PipeWire media target")?;
    session
        .start_media(media_generation)
        .await
        .context("start exact PipeWire media target")?;

    let first = session.get_statistics(media_generation).await?;
    ensure!(
        first.encoded_frames > 0,
        "Start returned before the backend produced a validated H.264 access unit"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    let flowing = session.get_statistics(media_generation).await?;
    ensure!(
        flowing.encoded_frames > first.encoded_frames,
        "backend encoded-frame statistics did not advance while streaming"
    );

    session
        .suspend_media(SuspendReason::OutputDisabled)
        .await
        .context("suspend backend media graph")?;
    let settled = session.get_statistics(media_generation).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let suspended = session.get_statistics(media_generation).await?;
    ensure!(
        suspended.encoded_frames == settled.encoded_frames,
        "backend continued consuming frames while suspended"
    );
    session
        .resume_media(media_generation)
        .await
        .context("resume backend media graph")?;
    let resumed = session.get_statistics(media_generation).await?;
    ensure!(
        resumed.encoded_frames > suspended.encoded_frames,
        "Resume returned before the backend received a new video frame"
    );

    drop(producer);
    let failure_deadline = Instant::now() + START_TIMEOUT;
    loop {
        if session.get_statistics(media_generation).await.is_err() {
            break;
        }
        ensure!(
            Instant::now() < failure_deadline,
            "backend reconnected to an unrelated source after its exact target disappeared"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(unrelated);

    session
        .stop_media(media_generation, StopReason::DisplayRemoved)
        .await
        .context("stop backend media graph")?;

    let crash_generation = NonZeroU64::new(4).unwrap();
    let crash_producer =
        GStreamerTestProducer::start_with_format("before-crash", 320, 240, 30).await?;
    let crash_remote = media_gate_remote(&remote_provider, session_id, crash_generation).await?;
    session
        .configure_media(
            vec![crash_remote],
            vec![media_gate_target(
                &crash_producer.node_name,
                crash_producer.object_serial.get(),
                session_id,
                crash_generation,
            )],
            media_gate_configuration(),
            crash_generation,
        )
        .await
        .context("configure pre-crash media generation")?;
    session
        .start_media(crash_generation)
        .await
        .context("start pre-crash media generation")?;
    ensure!(
        session
            .get_statistics(crash_generation)
            .await?
            .encoded_frames
            > 0,
        "pre-crash backend did not receive a frame"
    );
    let old_clients = wait_for_backend_pipewire_clients(1).await?;
    let old_client_serial = old_clients[0];
    launcher.kill_active_backend()?;
    drop(session);
    drop(crash_producer);

    match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Disconnected {
            connection_generation,
            reason,
            unavailable_inventory,
        } => {
            ensure!(connection_generation == 171, "wrong crashed generation");
            ensure!(
                reason == BackendDisconnectReason::ConnectionClosed,
                "wrong media-crash disconnect reason: {reason:?}"
            );
            ensure!(
                unavailable_inventory
                    .devices
                    .iter()
                    .all(|device| device.availability == DeviceAvailability::Unavailable),
                "backend crash did not preserve unavailable Device identities"
            );
        }
        event => bail!("unexpected media-crash supervisor event: {event:?}"),
    }
    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::ReconnectScheduled {
                next_connection_generation: 172,
                attempt: 1,
                delay: Duration::from_millis(10),
            },
        "media backend did not schedule one bounded reactivation"
    );
    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::Connecting {
                connection_generation: 172,
            },
        "media backend did not begin the replacement generation"
    );
    let replacement_discovery_generation = match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Connected {
            connection_generation,
            inventory,
            ..
        } => {
            ensure!(connection_generation == 172, "wrong replacement generation");
            inventory.discovery_generation
        }
        event => bail!("unexpected replacement media-backend event: {event:?}"),
    };
    wait_for_backend_pipewire_clients(0).await?;

    let replacement_producer =
        GStreamerTestProducer::start_with_format("after-crash", 320, 240, 30).await?;
    let replacement_session_id = "81234567-89ab-cdef-0123-456789abcdef";
    let replacement = supervisor
        .handle()
        .create_session(BackendSessionRequest::new(
            replacement_session_id,
            "living-room",
            SessionOptions {
                connection_generation: 172,
                discovery_generation: replacement_discovery_generation,
                session_generation: 2,
                requested_features: 0,
            },
        )?)
        .await?;
    replacement
        .prepare(media_gate_preparation_request())
        .await?;
    let replacement_generation = NonZeroU64::new(5).unwrap();
    let replacement_remote = media_gate_remote(
        &remote_provider,
        replacement_session_id,
        replacement_generation,
    )
    .await?;
    replacement
        .configure_media(
            vec![replacement_remote],
            vec![media_gate_target(
                &replacement_producer.node_name,
                replacement_producer.object_serial.get(),
                replacement_session_id,
                replacement_generation,
            )],
            media_gate_configuration(),
            replacement_generation,
        )
        .await
        .context("configure replacement backend with a fresh remote")?;
    replacement
        .start_media(replacement_generation)
        .await
        .context("start replacement backend media")?;
    ensure!(
        replacement
            .get_statistics(replacement_generation)
            .await?
            .encoded_frames
            > 0,
        "replacement backend did not receive a frame"
    );
    let replacement_clients = wait_for_backend_pipewire_clients(1).await?;
    ensure!(
        replacement_clients[0] != old_client_serial,
        "replacement backend reused the crashed PipeWire client"
    );
    replacement
        .stop_media(replacement_generation, StopReason::DisplayRemoved)
        .await
        .context("stop replacement backend media")?;
    replacement
        .stop(StopReason::DisplayRemoved)
        .await
        .context("stop replacement media-gate Device session")?;
    drop(replacement_producer);
    let report = supervisor.shutdown().await?;
    ensure!(
        report.graceful && report.last_connection_generation == Some(172),
        "media-gate backend shutdown failed: {report:?}"
    );
    Ok(())
}

async fn media_gate_remote(
    provider: &ClassifiedSocketRemoteProvider,
    session_id: &str,
    media_generation: NonZeroU64,
) -> anyhow::Result<zbus::zvariant::OwnedFd> {
    let remotes = provider
        .create_backend_remotes(session_id, "mock", media_generation, false)
        .await
        .context("mint untouched backend PipeWire connection")?;
    let (video, audio) = remotes.into_parts();
    ensure!(
        audio.is_none(),
        "video-only media gate minted an audio remote"
    );
    Ok(video.into_owned_fd().into())
}

fn media_gate_target(
    node_name: &str,
    object_serial: u64,
    session_id: &str,
    media_generation: NonZeroU64,
) -> PipeWireTarget {
    PipeWireTarget {
        kind: MediaKind::Video,
        node_name: node_name.into(),
        object_serial,
        session_id: session_id.into(),
        device_instance: "mock-living-room".into(),
        connector_id: 40,
        output_index: 0,
        media_generation: media_generation.get(),
        caps: "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1".into(),
    }
}

fn media_gate_configuration() -> MediaConfiguration {
    MediaConfiguration {
        video_profile_id: "h264-test".into(),
        audio_profile_id: None,
        mode: DisplayMode {
            width: 320,
            height: 240,
            refresh_millihz: 30_000,
            flags: 0,
        },
        video_bitrate: 1_000_000,
    }
}

fn media_gate_preparation_request() -> PreparationRequest {
    PreparationRequest {
        preparation_generation: 1,
        candidate_modes: vec![
            media_gate_configuration().mode,
            DisplayMode {
                width: 640,
                height: 480,
                refresh_millihz: 60_000,
                flags: 0,
            },
        ],
        video_profiles: vec![VideoProfile {
            profile_id: "h264-test".into(),
            codec: "h264".into(),
            max_width: 320,
            max_height: 240,
            max_refresh_millihz: 30_000,
        }],
        audio_profiles: Vec::new(),
        requested_features: 0,
    }
}

async fn run_real_display_setup(path: &Path) -> anyhow::Result<()> {
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    let validator = Arc::new(ExactRegistrationValidator::new("mock", "development"));
    let policy =
        BackendReconnectPolicy::new(0, Duration::ZERO, Duration::ZERO, Duration::from_secs(1))?;
    let connection = zbus::Connection::session()
        .await
        .context("connect to the graphical session bus")?;
    connection
        .request_name(pronk_dbus::BUS_NAME)
        .await
        .context("own the Pronk bus name used by Mutter authorization")?;
    let mut manager = ManagerActor::spawn(
        vec![BackendConfig::new(endpoint, 151, validator, policy)],
        Arc::new(MutterGrantProvider::new(connection.clone())),
    )?;
    let mut events = manager
        .take_events()
        .context("real display-setup manager event stream was already taken")?;
    for _ in 0..2 {
        timeout(METHOD_TIMEOUT, events.recv())
            .await
            .context("real display-setup inventory timed out")?
            .context("real display-setup inventory closed")?;
    }
    let snapshot = manager.handle().list_devices().await?;
    let device = snapshot
        .devices
        .iter()
        .find(|device| device.device_id == "living-room")
        .context("mock living-room Device is missing")?;
    let uid = fs::metadata("/proc/self")?.uid();
    let caller = PinnedCallerSession::pin_async(std::process::id(), uid, uid)
        .await?
        .into_process();
    let operation = manager
        .handle()
        .start_display_setup(DeviceSelection::from_device(device), None, caller, false)
        .await?;
    let mut status = operation.subscribe();
    ensure!(
        operation.snapshot().stage == DisplaySetupStage::Validating,
        "display setup did not return in Validating"
    );
    timeout(METHOD_TIMEOUT, async {
        while !status.borrow().stage.is_terminal() {
            status
                .changed()
                .await
                .context("display setup status closed")?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("display setup did not reach a terminal state")??;
    ensure!(
        status.borrow().stage == DisplaySetupStage::Added,
        "display setup did not reach Added"
    );
    let displays = manager.handle().list_displays().await?;
    let added = displays
        .iter()
        .find(|display| display.display_id == operation.display_id())
        .context("manager did not retain the added display")?;
    ensure!(
        added.device.device_id == "living-room"
            && added.prepared.generated_edid().display_name()
                == Some(added.device.display_name.as_str()),
        "added display identity differs from the selected Device"
    );
    let output_id = added.output.id.clone();
    let connector_id = added.output.connector_id;
    let attached = discover_castkms_outputs()?;
    ensure!(
        attached.iter().any(|output| {
            output.id == output_id
                && output.connector_id == connector_id
                && output.connection == OutputConnection::Connected
        }),
        "selected CastKMS output was not atomically attached"
    );
    manager.handle().remove_display(added.display_id).await?;
    ensure!(
        manager.handle().list_displays().await?.is_empty(),
        "removed cast display remains in the manager inventory"
    );
    let detached = discover_castkms_outputs()?;
    ensure!(
        detached.iter().any(|output| {
            output.id == output_id && output.connection == OutputConnection::Disconnected
        }),
        "removed cast display did not detach its exact output"
    );
    let report = manager.shutdown().await?;
    ensure!(
        report.errors.is_empty(),
        "manager shutdown failed: {report:?}"
    );
    connection.release_name(pronk_dbus::BUS_NAME).await?;
    Ok(())
}

async fn run_inventory_manager(path: &Path) -> anyhow::Result<()> {
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    let validator = Arc::new(ExactRegistrationValidator::new("mock", "development"));
    let policy =
        BackendReconnectPolicy::new(0, Duration::ZERO, Duration::ZERO, Duration::from_secs(1))?;
    let mut manager = ManagerActor::spawn_with_output_provider(
        vec![BackendConfig::new(endpoint, 101, validator, policy)],
        Arc::new(StaticOutputInventoryProvider {
            outputs: mock_outputs(),
        }),
        Arc::new(UnreachableGrantProvider),
    )?;
    let mut events = manager
        .take_events()
        .context("manager inventory event stream was already taken")?;

    for expected_revision in 1..=2 {
        let event = timeout(METHOD_TIMEOUT, events.recv())
            .await
            .context("manager device event timed out")?
            .context("manager device event stream closed")?;
        ensure!(
            matches!(
                event,
                InventoryEvent::DeviceAdded {
                    inventory_revision,
                    ..
                } if inventory_revision == expected_revision
            ),
            "unexpected manager inventory event: {event:?}"
        );
    }

    let snapshot = manager.handle().list_devices().await?;
    snapshot.validate()?;
    ensure!(snapshot.inventory_revision == 2, "wrong public revision");
    ensure!(snapshot.devices.len() == 2, "wrong public device count");
    ensure!(
        snapshot
            .devices
            .iter()
            .enumerate()
            .all(|(index, device)| device.connection_generation == 101
                && device.discovery_generation == 1
                && device.device_revision == index as u64 + 1),
        "public device generations do not match the backend snapshot"
    );

    let device = snapshot
        .devices
        .iter()
        .find(|device| device.device_id == "living-room")
        .context("mock living-room Device is missing")?;
    let mut stale_selection = DeviceSelection::from_device(device);
    stale_selection.device_revision += 1;
    ensure!(
        manager
            .handle()
            .resolve_device(stale_selection)
            .await
            .is_err(),
        "manager resolved a stale public Device selection"
    );

    let handle = manager.handle();
    let living_room_slot = handle
        .reserve_display_slot(DeviceSelection::from_device(device), None)
        .await?;
    ensure!(
        living_room_slot.device() == device && living_room_slot.output().id.output_index == 0,
        "manager reserved the wrong Device or CastKMS output"
    );
    ensure!(
        handle
            .reserve_display_slot(DeviceSelection::from_device(device), None)
            .await
            .is_err(),
        "manager reserved a second slot for the same Device"
    );
    let office_display = snapshot
        .devices
        .iter()
        .find(|device| device.device_id == "office-display")
        .context("mock office-display Device is missing")?;
    let office_slot = handle
        .reserve_display_slot(DeviceSelection::from_device(office_display), None)
        .await?;
    ensure!(
        office_slot.output().id.output_index == 1,
        "manager did not serialize output reservations"
    );
    drop(living_room_slot);
    drop(office_slot);
    let released_slot = handle
        .reserve_display_slot(DeviceSelection::from_device(device), None)
        .await?;
    ensure!(
        released_slot.output().id.output_index == 0,
        "dropping a pending slot did not release its exact reservation"
    );
    drop(released_slot);

    let selected = manager
        .handle()
        .resolve_device(DeviceSelection::from_device(device))
        .await?;
    ensure!(
        selected.device() == device,
        "manager resolved the wrong public Device"
    );
    let session = selected
        .create_session(
            "21234567-89ab-cdef-0123-456789abcdef",
            1,
            SESSION_FEATURE_AUDIO,
        )
        .await?;
    let capabilities = session.prepare(mock_preparation_request(1)).await?;
    ensure!(
        capabilities.display_identity.product_name.as_deref() == Some("Deterministic Mock Display"),
        "manager-routed preparation returned the wrong identity"
    );
    let pnp_resolver =
        PnpIdResolver::load_system(SYSTEM_PNP_IDS_PATH, &[], DEFAULT_SYNTHESIZER_PNP_ID)?;
    let prepared =
        PreparedCastDevice::from_capabilities(device.clone(), capabilities, &pnp_resolver, true)?;
    ensure!(
        prepared.pnp_resolution().pnp_id == DEFAULT_SYNTHESIZER_PNP_ID,
        "unresolved mock manufacturer did not use the synthesizer PNP ID"
    );
    ensure!(
        prepared.generated_edid().display_name() == Some(device.display_name.as_str()),
        "prepared mock assigned name was not embedded in DisplayID"
    );
    ensure!(
        prepared.generated_edid().edid().len() == 384,
        "ordinary mock identity did not produce base + DisplayID + CTA"
    );
    session.stop(StopReason::UserRequest).await?;

    let report = manager.shutdown().await?;
    ensure!(
        report.errors.is_empty(),
        "manager shutdown failed: {report:?}"
    );
    ensure!(
        report
            .backend_reports
            .get("mock")
            .is_some_and(|report| report.graceful),
        "manager did not gracefully stop its backend"
    );
    Ok(())
}

fn mock_outputs() -> Vec<CastKmsOutput> {
    (0..2)
        .map(|output_index| CastKmsOutput {
            id: CastKmsOutputId {
                device_path: PathBuf::from("/sys/devices/virtual/castkms"),
                output_index,
            },
            node_path: PathBuf::from("/dev/dri/card9"),
            device_major: 226,
            device_minor: 9,
            connector_id: 40 + output_index,
            connector_name: format!("Virtual-{}", output_index + 1),
            connection: OutputConnection::Disconnected,
        })
        .collect()
}

async fn run_supervised_preparation(path: &Path) -> anyhow::Result<()> {
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    let policy =
        BackendReconnectPolicy::new(0, Duration::ZERO, Duration::ZERO, Duration::from_secs(1))?;
    let mut supervisor = BackendSupervisor::spawn(
        endpoint,
        111,
        Arc::new(ExactRegistrationValidator::new("mock", "development")),
        policy,
    )?;
    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::Connecting {
                connection_generation: 111,
            },
        "session supervisor did not begin connecting"
    );
    let discovery_generation = match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Connected {
            connection_generation,
            inventory,
            ..
        } => {
            ensure!(
                connection_generation == 111,
                "wrong session connection generation"
            );
            inventory.discovery_generation
        }
        event => bail!("unexpected session supervisor event: {event:?}"),
    };

    let session_id = "11234567-89ab-cdef-0123-456789abcdef";
    let stale = BackendSessionRequest::new(
        session_id,
        "living-room",
        SessionOptions {
            connection_generation: 112,
            discovery_generation,
            session_generation: 1,
            requested_features: SESSION_FEATURE_AUDIO | SESSION_FEATURE_CONTROL,
        },
    )?;
    ensure!(
        matches!(
            supervisor.handle().create_session(stale).await,
            Err(BackendSessionError::StaleConnectionGeneration {
                expected: 111,
                actual: 112
            })
        ),
        "supervisor accepted a stale session selection"
    );

    let request = BackendSessionRequest::new(
        session_id,
        "living-room",
        SessionOptions {
            connection_generation: 111,
            discovery_generation,
            session_generation: 1,
            requested_features: SESSION_FEATURE_AUDIO | SESSION_FEATURE_CONTROL,
        },
    )?;
    let session = supervisor.handle().create_session(request).await?;
    ensure!(session.backend_id() == "mock", "wrong prepared backend ID");
    ensure!(
        session.device_id() == "living-room",
        "wrong prepared device ID"
    );
    let mut preparation = mock_preparation_request(1);
    preparation.requested_features |= SESSION_FEATURE_CONTROL;
    let capabilities = session.prepare(preparation).await?;
    ensure!(
        capabilities.display_identity.product_name.as_deref() == Some("Deterministic Mock Display"),
        "supervised preparation returned the wrong identity"
    );
    ensure!(
        capabilities.features & SESSION_FEATURE_CONTROL != 0,
        "mock backend did not retain the requested control feature"
    );
    session
        .transmit_control(ControlOperation {
            session_generation: 1,
            kind: ControlKind::Volume,
            code: Some("absolute".into()),
            value: 75,
        })
        .await
        .context("round-trip normalized control completion")?;
    run_supervised_media_lifecycle(session, session_id).await?;

    let report = supervisor.shutdown().await?;
    ensure!(
        report.graceful,
        "session supervisor shutdown failed: {report:?}"
    );
    Ok(())
}

async fn run_supervised_media_lifecycle(
    session: pronk_backend_host::BackendSessionHandle,
    session_id: &str,
) -> anyhow::Result<()> {
    let media_generation = NonZeroU64::new(1).unwrap();
    let (remote, observer) = StdUnixStream::pair().context("create mock PipeWire remote")?;
    observer
        .set_nonblocking(true)
        .context("make mock PipeWire observer nonblocking")?;
    let mut observer = tokio::net::UnixStream::from_std(observer)
        .context("register mock PipeWire observer with Tokio")?;
    let mut session = BackendDeviceSession::new(session);
    session
        .configure_media(DeviceMediaSetup {
            media_generation,
            endpoints: vec![DeviceMediaEndpoint {
                remote: std::os::fd::OwnedFd::from(remote),
                target: DeviceMediaTarget {
                    kind: DeviceMediaKind::Video,
                    node_name: format!("pronk.video.{session_id}.1"),
                    object_serial: NonZeroU64::new(101).unwrap(),
                    session_id: session_id.into(),
                    device_instance: "mock-living-room".into(),
                    connector_id: NonZeroU32::new(40).unwrap(),
                    output_index: 0,
                    media_generation,
                    caps: "video/x-raw,format=BGRx,width=1920,height=1080,framerate=60/1".into(),
                },
            }],
            configuration: DeviceMediaConfiguration {
                video_profile_id: "h264-high".into(),
                audio_profile_id: None,
                mode: RoutedMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
                video_bitrate: NonZeroU64::new(8_000_000).unwrap(),
            },
        })
        .await
        .context("configure supervised mock media")?;

    let mut byte = [0_u8; 1];
    ensure!(
        timeout(Duration::from_millis(50), observer.read(&mut byte))
            .await
            .is_err(),
        "backend did not retain the transferred PipeWire remote"
    );
    session
        .start_media(media_generation)
        .await
        .context("start supervised mock media")?;
    session
        .suspend_media(media_generation, DeviceMediaSuspendReason::OutputDisabled)
        .await
        .context("suspend supervised mock media")?;
    session
        .resume_media(media_generation)
        .await
        .context("resume supervised mock media")?;
    session
        .stop_media(media_generation, DeviceMediaStopReason::OutputDisabled)
        .await
        .context("stop supervised mock media")?;
    let read = timeout(METHOD_TIMEOUT, observer.read(&mut byte))
        .await
        .context("transferred PipeWire remote remained open after StopMedia")?
        .context("read mock PipeWire observer after StopMedia")?;
    ensure!(read == 0, "backend wrote unexpected mock PipeWire data");
    Box::new(session)
        .stop(DeviceSessionStopReason::DisplayRemoved)
        .await
        .context("stop supervised mock Device session")?;
    Ok(())
}

fn mock_preparation_request(preparation_generation: u64) -> PreparationRequest {
    PreparationRequest {
        preparation_generation,
        candidate_modes: vec![
            DisplayMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                flags: 0,
            },
            DisplayMode {
                width: 640,
                height: 480,
                refresh_millihz: 60_000,
                flags: 0,
            },
        ],
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

async fn run_valid_connection(path: &Path, connection_generation: u64) -> anyhow::Result<()> {
    let connection = connect_backend(path, connection_generation).await?;
    ensure!(
        connection.info().backend_id == "mock",
        "unexpected backend ID"
    );
    ensure!(
        connection.negotiated_minor() == 0,
        "unexpected negotiated minor"
    );

    let backend = Backend1Proxy::new(connection.connection())
        .await
        .context("create Backend1 proxy")?;
    let mut discovery = connection
        .start_discovery()
        .await
        .context("start managed discovery")?;
    let snapshot = discovery.initial().clone();
    let discovery_generation = snapshot.discovery_generation;
    ensure!(
        discovery_generation == 1,
        "unexpected initial discovery generation"
    );
    ensure!(snapshot.revision == 2, "unexpected snapshot revision");
    ensure!(snapshot.devices.len() == 2, "unexpected device count");
    ensure!(
        timeout(Duration::from_millis(100), discovery.next_notification())
            .await
            .is_err(),
        "signals covered by the initial snapshot produced a duplicate update"
    );

    let session_id = "01234567-89ab-cdef-0123-456789abcdef";
    let session_options = SessionOptions {
        connection_generation,
        discovery_generation,
        session_generation: 7,
        requested_features: SESSION_FEATURE_AUDIO,
    };
    let mut stale_options = session_options.clone();
    stale_options.connection_generation += 1;
    ensure!(
        timeout(
            METHOD_TIMEOUT,
            backend.create_session(session_id.into(), "living-room".into(), stale_options,),
        )
        .await
        .context("stale CreateSession timed out")?
        .is_err(),
        "stale connection generation was accepted by CreateSession"
    );
    let session_path = timeout(
        METHOD_TIMEOUT,
        backend.create_session(session_id.into(), "living-room".into(), session_options),
    )
    .await
    .context("CreateSession timed out")??;
    let session = BackendSession1Proxy::builder(connection.connection())
        .path(session_path)?
        .build()
        .await?;
    let preparation = PreparationRequest {
        preparation_generation: 11,
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
    };
    let capabilities = timeout(METHOD_TIMEOUT, session.prepare(preparation))
        .await
        .context("Prepare timed out")??;
    capabilities
        .validate()
        .context("validate mock device capabilities")?;
    ensure!(
        capabilities.preparation_generation == 11,
        "Prepare returned a stale generation"
    );
    ensure!(
        capabilities.display_identity.product_name.as_deref() == Some("Deterministic Mock Display"),
        "mock returned unexpected authenticated identity"
    );
    timeout(METHOD_TIMEOUT, session.stop(StopReason::UserRequest))
        .await
        .context("BackendSession1 Stop timed out")??;

    ensure!(
        timeout(
            METHOD_TIMEOUT,
            backend.stop_discovery(discovery_generation + 1)
        )
        .await
        .context("stale StopDiscovery timed out")?
        .is_err(),
        "stale discovery generation was accepted"
    );
    discovery.stop().await.context("stop managed discovery")?;
    connection.shutdown().await.context("shutdown backend")?;
    timeout(METHOD_TIMEOUT, connection.wait_for_eof())
        .await
        .context("backend did not close its P2P stream")??;
    Ok(())
}

async fn run_stale_major_connection(path: &Path) -> anyhow::Result<()> {
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    let error = BackendConnection::connect(
        endpoint,
        99,
        Arc::new(ExactRegistrationValidator::new("mock", "development")),
    )
    .await
    .expect_err("protocol major 2 unexpectedly connected");
    ensure!(
        matches!(error, BackendConnectError::RegistrationRejected(ref message) if message.contains("protocol major 2")),
        "unexpected stale-major error: {error}"
    );
    Ok(())
}

async fn run_revision_gap_connection(path: &Path) -> anyhow::Result<()> {
    let connection = connect_backend(path, 71).await?;
    let mut discovery = connection.start_discovery().await?;
    ensure!(discovery.initial().revision == 2, "unexpected gap baseline");
    let notification = timeout(METHOD_TIMEOUT, discovery.next_notification())
        .await
        .context("revision-gap resnapshot timed out")?
        .context("revision-gap discovery actor stopped")?;
    match notification {
        DiscoveryNotification::Resynchronized { reason, snapshot } => {
            ensure!(
                reason.contains("expected 3, received 4"),
                "unexpected resnapshot reason: {reason}"
            );
            ensure!(
                snapshot.revision == 4,
                "resnapshot did not advance revision"
            );
            ensure!(
                snapshot.devices.iter().any(|device| {
                    device.device_id == "living-room"
                        && device.display_name == "Living Room TV (updated)"
                }),
                "resnapshot did not install changed device"
            );
        }
        other => anyhow::bail!("unexpected revision-gap notification: {other:?}"),
    }
    discovery.stop().await?;
    connection.shutdown().await?;
    timeout(METHOD_TIMEOUT, connection.wait_for_eof())
        .await
        .context("gap backend did not close")??;
    Ok(())
}

async fn run_unsolicited_eof_connection(path: &Path) -> anyhow::Result<()> {
    let connection = connect_backend(path, 81).await?;
    let mut discovery = connection.start_discovery().await?;
    let notification = timeout(METHOD_TIMEOUT, discovery.next_notification())
        .await
        .context("unsolicited EOF notification timed out")?
        .context("EOF discovery actor stopped without notification")?;
    ensure!(
        notification == DiscoveryNotification::ConnectionClosed,
        "unexpected EOF notification: {notification:?}"
    );
    timeout(METHOD_TIMEOUT, connection.wait_for_eof())
        .await
        .context("unsolicited backend EOF timed out")??;
    Ok(())
}

async fn run_supervised_reactivation(path: &Path) -> anyhow::Result<()> {
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    let validator = Arc::new(ExactRegistrationValidator::new("mock", "development"));
    let policy = BackendReconnectPolicy::new(
        1,
        Duration::from_millis(10),
        Duration::from_millis(10),
        Duration::from_secs(1),
    )?;
    let mut supervisor = BackendSupervisor::spawn(endpoint, 91, validator, policy)?;

    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::Connecting {
                connection_generation: 91,
            },
        "supervisor did not begin generation 91"
    );
    match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Connected {
            connection_generation,
            inventory,
            ..
        } => {
            ensure!(connection_generation == 91, "wrong first generation");
            ensure!(inventory.devices.len() == 2, "wrong first inventory");
        }
        event => bail!("unexpected first supervisor connection event: {event:?}"),
    }
    match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Disconnected {
            connection_generation,
            reason,
            unavailable_inventory,
        } => {
            ensure!(connection_generation == 91, "wrong disconnected generation");
            ensure!(
                reason == BackendDisconnectReason::ConnectionClosed,
                "wrong supervisor disconnect reason: {reason:?}"
            );
            ensure!(
                unavailable_inventory.devices.len() == 2
                    && unavailable_inventory
                        .devices
                        .iter()
                        .all(|device| device.availability == DeviceAvailability::Unavailable),
                "supervisor did not preserve and mark its inventory unavailable"
            );
        }
        event => bail!("unexpected supervisor disconnect event: {event:?}"),
    }
    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::ReconnectScheduled {
                next_connection_generation: 92,
                attempt: 1,
                delay: Duration::from_millis(10),
            },
        "supervisor did not schedule the bounded retry"
    );
    ensure!(
        next_supervisor_event(&mut supervisor).await?
            == BackendSupervisorEvent::Connecting {
                connection_generation: 92,
            },
        "supervisor did not activate generation 92"
    );
    match next_supervisor_event(&mut supervisor).await? {
        BackendSupervisorEvent::Connected {
            connection_generation,
            inventory,
            ..
        } => {
            ensure!(connection_generation == 92, "wrong replacement generation");
            ensure!(inventory.devices.len() == 2, "wrong replacement inventory");
        }
        event => bail!("unexpected replacement supervisor event: {event:?}"),
    }
    ensure!(
        matches!(
            supervisor.handle().retry_now().await,
            Err(BackendRetryError::AlreadyConnected)
        ),
        "manual retry replaced a healthy backend"
    );
    let report = supervisor.shutdown().await?;
    ensure!(report.graceful, "supervisor shutdown failed: {report:?}");
    ensure!(
        report.last_connection_generation == Some(92),
        "supervisor shutdown reported the wrong generation"
    );
    Ok(())
}

async fn next_supervisor_event(
    supervisor: &mut BackendSupervisor,
) -> anyhow::Result<BackendSupervisorEvent> {
    timeout(METHOD_TIMEOUT, supervisor.next_event())
        .await
        .context("backend supervisor event timed out")?
        .context("backend supervisor event stream closed")
}

async fn connect_backend(path: &Path, generation: u64) -> anyhow::Result<BackendConnection> {
    let endpoint = BackendEndpoint::new("mock", path, "pronk-backend-mock@.service")?;
    BackendConnection::connect(
        endpoint,
        generation,
        Arc::new(ExactRegistrationValidator::new("mock", "development")),
    )
    .await
    .context("connect registered backend")
}

#[derive(Debug)]
struct ActivationLauncher {
    child: Option<Child>,
    socket_path: PathBuf,
    backend_path: PathBuf,
}

impl ActivationLauncher {
    fn start(
        socket_activate: &Path,
        backend: &Path,
        socket_path: &Path,
        protocol_major: Option<&str>,
        discovery_scenario: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::start_with_media_mode(
            socket_activate,
            backend,
            socket_path,
            protocol_major,
            discovery_scenario,
            "retain-for-protocol-test",
        )
    }

    fn start_gstreamer(
        socket_activate: &Path,
        backend: &Path,
        socket_path: &Path,
    ) -> anyhow::Result<Self> {
        Self::start_with_media_mode(
            socket_activate,
            backend,
            socket_path,
            None,
            None,
            "gstreamer",
        )
    }

    fn start_with_media_mode(
        socket_activate: &Path,
        backend: &Path,
        socket_path: &Path,
        protocol_major: Option<&str>,
        discovery_scenario: Option<&str>,
        media_mode: &str,
    ) -> anyhow::Result<Self> {
        remove_stale_socket(socket_path)?;
        let backend_path = fs::canonicalize(backend)
            .with_context(|| format!("resolve mock backend {}", backend.display()))?;
        let mut command = Command::new(socket_activate);
        command
            .arg(format!("--listen={}", socket_path.display()))
            .arg("--accept")
            .arg(format!("--fdname={BACKEND_CONTROL_FD_NAME}"))
            .arg("--setenv=PIPEWIRE_REMOTE=ambient-remote-must-not-survive")
            .arg("--setenv=PRONK_BACKEND_INSTANCE=mock")
            .arg("--setenv=INVOCATION_ID=development")
            .arg("--setenv=PRONK_BACKEND_ALLOW_UNMANAGED_PEER=1")
            .arg(format!(
                "--setenv=PRONK_BACKEND_MOCK_MEDIA_MODE={media_mode}"
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        pass_gstreamer_diagnostic_environment(&mut command);
        if let Some(protocol_major) = protocol_major {
            command.arg(format!(
                "--setenv=PRONK_BACKEND_MOCK_PROTOCOL_MAJOR={protocol_major}"
            ));
        }
        if let Some(discovery_scenario) = discovery_scenario {
            command.arg(format!(
                "--setenv=PRONK_BACKEND_MOCK_DISCOVERY_SCENARIO={discovery_scenario}"
            ));
        }
        command.arg(&backend_path);
        let child = command.spawn().context("start systemd-socket-activate")?;
        Ok(Self {
            child: Some(child),
            socket_path: socket_path.to_owned(),
            backend_path,
        })
    }

    async fn wait_until_listening(&mut self) -> anyhow::Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if self.socket_path.exists() {
                return Ok(());
            }
            if let Some(status) = self
                .child_mut()
                .try_wait()
                .context("query activation launcher")?
            {
                bail!("systemd-socket-activate exited early with {status}");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {}", self.socket_path.display());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("launcher has been stopped")
    }

    fn kill_active_backend(&self) -> anyhow::Result<u32> {
        let launcher = self.child.as_ref().context("launcher has been stopped")?;
        let launcher_pid = launcher.id();
        let children_path = format!("/proc/{launcher_pid}/task/{launcher_pid}/children");
        let children =
            fs::read_to_string(&children_path).with_context(|| format!("read {children_path}"))?;
        let pids: Vec<u32> = children
            .split_whitespace()
            .map(|value| value.parse().context("parse activation child PID"))
            .collect::<Result<_, _>>()?;
        ensure!(
            pids.len() == 1,
            "activation launcher has {} live children; expected one exact backend",
            pids.len()
        );
        let pid = pids[0];
        let executable = fs::read_link(format!("/proc/{pid}/exe"))
            .with_context(|| format!("resolve activation child {pid} executable"))?;
        ensure!(
            executable == self.backend_path,
            "activation child {pid} is {executable:?}, not {:?}",
            self.backend_path
        );
        kill(
            Pid::from_raw(i32::try_from(pid).context("backend PID exceeds pid_t")?),
            Signal::SIGKILL,
        )
        .with_context(|| format!("kill exact mock backend child {pid}"))?;
        Ok(pid)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child.kill().context("stop activation launcher")?;
            }
            child.wait().context("reap activation launcher")?;
        }
        remove_stale_socket(&self.socket_path)
    }
}

impl Drop for ActivationLauncher {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = remove_stale_socket(&self.socket_path);
    }
}

fn temporary_socket_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pronk-backend-p2p-{}-{label}.sock",
        std::process::id()
    ))
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn parse_arguments() -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let socket_activate = arguments
        .next()
        .context("missing systemd-socket-activate path")?;
    let mock_backend = arguments.next().context("missing mock backend path")?;
    ensure!(arguments.next().is_none(), "unexpected extra argument");
    Ok((socket_activate, mock_backend))
}
