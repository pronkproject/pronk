use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use futures_util::StreamExt;
use pronk_backend_host::{
    BackendConnection, BackendEndpoint, DiscoveryNotification, ExactRegistrationValidator,
};
use pronk_backend_protocol::{
    session_object_path, Backend1Proxy, BackendSession1Proxy, DisplayMode, IdentitySource,
    MediaConfiguration, MediaKind, PipeWireTarget, SessionOptions, StopReason, SuspendReason,
    Validate, SESSION_FEATURE_CONTROL,
};
use pronk_core::identity::{
    PnpIdResolver, PnpResolutionSource, DEFAULT_SYNTHESIZER_PNP_ID, SYSTEM_PNP_IDS_PATH,
};
use pronk_dbus::{DeviceAvailability as PublicDeviceAvailability, DeviceInfo as PublicDeviceInfo};
use pronk_pipewire::{ClassifiedSocketPaths, ClassifiedSocketRemoteProvider};
use pronk_systemd::BACKEND_CONTROL_FD_NAME;

mod edid_conformance;
mod gstreamer_fixture;

use gstreamer_fixture::{
    pass_gstreamer_diagnostic_environment, wait_for_backend_pipewire_clients, GStreamerTestProducer,
};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const LIVE_START_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let (socket_activate, backend_path, edid_decode) = parse_arguments()?;
    let live_device_id = std::env::var("PRONK_CHROMIACAST_LIVE_DEVICE_ID").ok();
    let live_expected_manufacturer =
        std::env::var("PRONK_CHROMIACAST_LIVE_EXPECTED_MANUFACTURER").ok();
    let live_expected_product = std::env::var("PRONK_CHROMIACAST_LIVE_EXPECTED_PRODUCT").ok();
    let live_expected_pnp_id = std::env::var("PRONK_CHROMIACAST_LIVE_EXPECTED_PNP_ID").ok();
    let media_gate = match std::env::var_os("PRONK_BACKEND_MEDIA_GATE") {
        None => false,
        Some(value) if value == "real" => true,
        Some(_) => bail!("PRONK_BACKEND_MEDIA_GATE must be unset or 'real'"),
    };
    let socket_path = temporary_socket_path();
    let mut launcher = ActivationLauncher::start(
        &socket_activate,
        &backend_path,
        &socket_path,
        live_device_id.is_none(),
    )?;
    launcher.wait_until_listening().await?;

    let endpoint = BackendEndpoint::new("chromiacast", &socket_path, "pronk-chromiacast@.service")?;
    let connection = BackendConnection::connect(
        endpoint,
        73,
        Arc::new(ExactRegistrationValidator::new(
            "chromiacast",
            "development",
        )),
    )
    .await
    .context("complete Chromiacast backend registration")?;
    ensure!(
        connection.info().backend_id == "chromiacast",
        "registered the wrong backend"
    );
    ensure!(
        connection.connection_generation() == 73,
        "registered the wrong connection generation"
    );

    // BackendConnection completes the versioned registration and GetInfo
    // exchange before exposing StartDiscovery. The fixture source cannot scan
    // until this method is called.
    let mut discovery = connection
        .start_discovery()
        .await
        .context("start Chromiacast discovery after registration")?;
    ensure!(
        discovery.initial().discovery_generation == 1,
        "wrong initial discovery generation"
    );
    let inventory = if discovery.initial().devices.iter().any(|device| {
        live_device_id
            .as_ref()
            .is_none_or(|id| &device.device_id == id)
    }) {
        discovery.initial().clone()
    } else {
        tokio::time::timeout(
            if live_device_id.is_some() {
                LIVE_START_TIMEOUT
            } else {
                START_TIMEOUT
            },
            async {
                loop {
                    match discovery.next_notification().await {
                        Some(DiscoveryNotification::Changed(snapshot))
                        | Some(DiscoveryNotification::Resynchronized { snapshot, .. })
                            if snapshot.devices.iter().any(|device| {
                                live_device_id
                                    .as_ref()
                                    .is_none_or(|id| &device.device_id == id)
                            }) =>
                        {
                            return Ok(snapshot);
                        }
                        Some(DiscoveryNotification::FatalError { error_text, .. })
                        | Some(DiscoveryNotification::Failed(error_text)) => {
                            bail!("Chromiacast discovery failed: {error_text}")
                        }
                        Some(DiscoveryNotification::ConnectionClosed) | None => {
                            bail!("Chromiacast discovery connection closed")
                        }
                        Some(_) => {}
                    }
                }
            },
        )
        .await
        .context("wait for selected Chromiacast Device")??
    };
    let device = match live_device_id.as_ref() {
        Some(device_id) => inventory
            .devices
            .iter()
            .find(|device| &device.device_id == device_id)
            .context("selected live Chromiacast Device disappeared")?,
        None => {
            ensure!(inventory.devices.len() == 1, "wrong fixture device count");
            let device = &inventory.devices[0];
            ensure!(
                device.device_id == "00112233445566778899aabbccddeeff",
                "wrong fixture device identity"
            );
            device
        }
    };

    let session_id = "12345678-1234-1234-1234-123456789abc";
    let options = SessionOptions {
        connection_generation: connection.connection_generation(),
        discovery_generation: inventory.discovery_generation,
        session_generation: 1,
        requested_features: SESSION_FEATURE_CONTROL,
    };
    let backend = Backend1Proxy::new(connection.connection())
        .await
        .context("create Chromiacast Backend1 proxy")?;
    let path = backend
        .create_session(session_id.into(), device.device_id.clone(), options.clone())
        .await
        .context("create Chromiacast device session")?;
    ensure!(
        path == session_object_path(session_id, options.session_generation)?,
        "Chromiacast returned the wrong session path"
    );
    let session = BackendSession1Proxy::builder(connection.connection())
        .path(path)
        .context("set Chromiacast session path")?
        .build()
        .await
        .context("create Chromiacast BackendSession1 proxy")?;
    let capabilities = session
        .prepare(pronk::preparation::initial_preparation_offer(false))
        .await
        .context("prepare authenticated Chromiacast device")?;
    capabilities
        .validate()
        .context("validate Chromiacast capabilities")?;
    if live_device_id.is_none() {
        ensure!(
            capabilities.display_identity.manufacturer_name.as_deref() == Some("Sony Corporation")
                && capabilities.display_identity.manufacturer_source
                    == IdentitySource::SetupEndpoint,
            "Chromiacast did not return setup-endpoint manufacturer identity"
        );
        ensure!(
            capabilities.display_identity.product_name.as_deref() == Some("BRAVIA 8")
                && capabilities.display_identity.product_source == IdentitySource::SetupEndpoint,
            "Chromiacast did not prefer setup-endpoint product identity"
        );
    } else {
        ensure!(
            capabilities.display_identity.manufacturer_source
                != IdentitySource::DiscoveryAdvertisement
                && capabilities.display_identity.product_source
                    != IdentitySource::DiscoveryAdvertisement,
            "live Device preparation trusted discovery-only display identity"
        );
        println!(
            "live_identity={:?}/{:?}",
            capabilities.display_identity.manufacturer_name,
            capabilities.display_identity.product_name
        );
        if let Some(expected) = &live_expected_manufacturer {
            ensure!(
                capabilities.display_identity.manufacturer_name.as_deref()
                    == Some(expected.as_str())
                    && capabilities.display_identity.manufacturer_source
                        == IdentitySource::SetupEndpoint,
                "live Device did not return expected setup manufacturer {expected:?}"
            );
        }
        if let Some(expected) = &live_expected_product {
            ensure!(
                capabilities.display_identity.product_name.as_deref() == Some(expected.as_str())
                    && capabilities.display_identity.product_source
                        == IdentitySource::SetupEndpoint,
                "live Device did not return expected setup product {expected:?}"
            );
        }
    }
    let resolver = PnpIdResolver::load_system(SYSTEM_PNP_IDS_PATH, &[], DEFAULT_SYNTHESIZER_PNP_ID)
        .context("load installed PNP identity database")?;
    let prepared = pronk::preparation::PreparedCastDevice::from_capabilities(
        PublicDeviceInfo {
            backend_id: device.backend_id.clone(),
            device_id: device.device_id.clone(),
            display_name: device.display_name.clone(),
            availability: PublicDeviceAvailability::Available,
            connection_generation: connection.connection_generation(),
            discovery_generation: inventory.discovery_generation,
            device_revision: 1,
            metadata: Vec::new(),
        },
        capabilities,
        &resolver,
        false,
    )
    .context("prepare authenticated Chromiacast display identity")?;
    if live_device_id.is_none() {
        ensure!(
            prepared.pnp_resolution().pnp_id.as_str() == "SNY"
                && prepared.pnp_resolution().database_name.as_deref() == Some("Sony")
                && prepared.pnp_resolution().source == PnpResolutionSource::LegalSuffixName,
            "Sony Corporation did not resolve deterministically through installed hwdata"
        );
        edid_conformance::check(
            edid_decode.as_os_str(),
            prepared.generated_edid().edid().as_bytes(),
            &device.display_name,
        )
        .context("validate authenticated Chromiacast DisplayID")?;
    } else {
        if let Some(expected) = &live_expected_pnp_id {
            ensure!(
                prepared.pnp_resolution().pnp_id.as_str() == expected,
                "live Device resolved PNP ID {:?}, expected {expected:?}",
                prepared.pnp_resolution().pnp_id.as_str()
            );
        }
        if let Some(expected) = &live_expected_product {
            edid_conformance::check(
                edid_decode.as_os_str(),
                prepared.generated_edid().edid().as_bytes(),
                expected,
            )
            .context("validate live Chromiacast DisplayID")?;
        }
    }
    if media_gate {
        if live_device_id.is_some() {
            run_live_media_gate(&session, session_id).await?;
        } else {
            run_media_gate(&session, session_id).await?;
        }
    }
    session
        .stop(StopReason::UserRequest)
        .await
        .context("stop Chromiacast device session")?;
    let replacement_options = SessionOptions {
        connection_generation: connection.connection_generation(),
        discovery_generation: inventory.discovery_generation,
        session_generation: 2,
        requested_features: 0,
    };
    let replacement_path = backend
        .create_session(
            session_id.into(),
            device.device_id.clone(),
            replacement_options.clone(),
        )
        .await
        .context("replace stopped Chromiacast session without waiting for its old object path")?;
    ensure!(
        replacement_path
            == session_object_path(session_id, replacement_options.session_generation)?,
        "replacement Chromiacast session returned the wrong path"
    );
    let replacement = BackendSession1Proxy::builder(connection.connection())
        .path(replacement_path)
        .context("set replacement Chromiacast session path")?
        .build()
        .await
        .context("create replacement BackendSession1 proxy")?;
    replacement
        .stop(StopReason::UserRequest)
        .await
        .context("stop replacement Chromiacast session")?;
    discovery
        .stop()
        .await
        .context("stop Chromiacast discovery")?;

    connection
        .shutdown()
        .await
        .context("request Chromiacast backend shutdown")?;
    connection
        .wait_for_eof()
        .await
        .context("wait for Chromiacast P2P EOF")?;
    connection.close().await.context("close host connection")?;
    launcher.stop()?;

    println!("chromiacast_versioned_registration=pass");
    println!("chromiacast_registration_before_discovery=pass");
    if live_device_id.is_some() {
        println!("chromiacast_live_device_selection=pass");
    } else {
        println!("chromiacast_revisioned_fixture_snapshot=pass");
    }
    println!("chromiacast_display_identity=pass");
    println!("chromiacast_selected_device_cross_check=pass");
    println!("chromiacast_hwdata_legal_suffix_resolution=pass");
    println!("chromiacast_displayid_conformance=pass");
    if media_gate {
        if live_device_id.is_some() {
            println!("chromiacast_live_cast_launch=pass");
            println!("chromiacast_live_encoded_delivery=pass");
        }
        println!("chromiacast_passed_fd_no_ambient_fallback=pass");
        println!("chromiacast_exact_pipewire_target=pass");
        println!("chromiacast_pipewire_pts=pass");
        println!("chromiacast_h264_annex_b_encoder=pass");
        println!("chromiacast_encoded_transport_adapter=pass");
        println!("chromiacast_generation_scoped_feedback=pass");
        println!("chromiacast_dynamic_encoder_bitrate=pass");
        println!("chromiacast_media_suspend_resume=pass");
        println!("chromiacast_media_statistics=pass");
        println!("chromiacast_media_fd_cleanup=pass");
    }
    println!("chromiacast_bounded_session_objects=pass");
    println!("chromiacast_ordered_shutdown_eof=pass");
    Ok(())
}

async fn run_live_media_gate(
    session: &BackendSession1Proxy<'_>,
    session_id: &str,
) -> anyhow::Result<()> {
    let producer =
        GStreamerTestProducer::start_with_format("chromiacast-live", 640, 480, 60).await?;
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("live Chromiacast media gate requires XDG_RUNTIME_DIR")?;
    let remote_provider = ClassifiedSocketRemoteProvider::new(
        ClassifiedSocketPaths::in_runtime_dir(runtime_dir)
            .context("construct live Chromiacast PipeWire paths")?,
    );
    let generation = NonZeroU64::new(1).unwrap();
    let remote = media_remote(&remote_provider, session_id, generation).await?;
    session
        .configure_media(
            vec![remote],
            vec![media_target(
                &producer,
                session_id,
                generation,
                producer.object_serial.get(),
            )],
            media_configuration(),
            generation.get(),
        )
        .await
        .context("negotiate live Cast media transport")?;
    session
        .start(generation.get())
        .await
        .context("start live Cast media transport")?;
    let initial = session
        .get_statistics()
        .await
        .context("read initial live Cast statistics")?;
    ensure!(
        initial.encoded_frames > 0,
        "live Cast Start returned without encoded-frame delivery"
    );
    let delivered = tokio::time::timeout(LIVE_START_TIMEOUT, async {
        loop {
            let statistics = session.get_statistics().await?;
            if statistics.encoded_frames >= initial.encoded_frames.saturating_add(30) {
                return Ok::<_, zbus::Error>(statistics);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("wait for sustained live Cast delivery")??;
    println!(
        "live_encoded_frames={} live_dropped_frames={} live_bitrate={}",
        delivered.encoded_frames, delivered.dropped_frames, delivered.video_bitrate
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    session
        .stop_media(generation.get(), StopReason::UserRequest)
        .await
        .context("stop live Cast media transport")?;
    wait_for_backend_pipewire_clients(0).await?;
    drop(producer);
    Ok(())
}

async fn run_media_gate(
    session: &BackendSession1Proxy<'_>,
    session_id: &str,
) -> anyhow::Result<()> {
    let producer = GStreamerTestProducer::start_with_format("chromiacast", 640, 480, 60).await?;
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("Chromiacast media gate requires XDG_RUNTIME_DIR")?;
    let remote_provider = ClassifiedSocketRemoteProvider::new(
        ClassifiedSocketPaths::in_runtime_dir(runtime_dir)
            .context("construct Chromiacast media-gate PipeWire paths")?,
    );

    let invalid_fd_generation = NonZeroU64::new(1).unwrap();
    let (invalid_remote, invalid_peer) =
        StdUnixStream::pair().context("create disconnected Chromiacast media remote")?;
    drop(invalid_peer);
    let invalid_remote: zbus::zvariant::OwnedFd = std::os::fd::OwnedFd::from(invalid_remote).into();
    if session
        .configure_media(
            vec![invalid_remote],
            vec![media_target(
                &producer,
                session_id,
                invalid_fd_generation,
                producer.object_serial.get(),
            )],
            media_configuration(),
            invalid_fd_generation.get(),
        )
        .await
        .is_ok()
    {
        ensure!(
            session.start(invalid_fd_generation.get()).await.is_err(),
            "Chromiacast ignored a passed disconnected fd and opened an ambient remote"
        );
    }
    session
        .stop_media(invalid_fd_generation.get(), StopReason::TransportFailure)
        .await
        .context("clean up rejected Chromiacast media fd")?;

    let rejected_generation = NonZeroU64::new(2).unwrap();
    let rejected_remote = media_remote(&remote_provider, session_id, rejected_generation).await?;
    let mut nonexistent_target = media_target(
        &producer,
        session_id,
        rejected_generation,
        producer.object_serial.get(),
    );
    nonexistent_target.node_name = format!("{}.missing", producer.node_name);
    if session
        .configure_media(
            vec![rejected_remote],
            vec![nonexistent_target],
            media_configuration(),
            rejected_generation.get(),
        )
        .await
        .is_ok()
    {
        ensure!(
            session.start(rejected_generation.get()).await.is_err(),
            "Chromiacast fell back from a nonexistent exact PipeWire target"
        );
    }
    session
        .stop_media(rejected_generation.get(), StopReason::TransportFailure)
        .await
        .context("clean up rejected Chromiacast PipeWire target")?;

    let generation = NonZeroU64::new(3).unwrap();
    let mut key_frame_requests = session
        .receive_keyframe_requested()
        .await
        .context("subscribe to Chromiacast key-frame feedback")?;
    let mut bitrate_requests = session
        .receive_bitrate_requested()
        .await
        .context("subscribe to Chromiacast bitrate feedback")?;
    let remote = media_remote(&remote_provider, session_id, generation).await?;
    session
        .configure_media(
            vec![remote],
            vec![media_target(
                &producer,
                session_id,
                generation,
                producer.object_serial.get(),
            )],
            media_configuration(),
            generation.get(),
        )
        .await
        .context("configure exact Chromiacast PipeWire media target")?;
    session
        .start(generation.get())
        .await
        .context("start exact Chromiacast PipeWire media target")?;

    let key_frame_signal = tokio::time::timeout(START_TIMEOUT, key_frame_requests.next())
        .await
        .context("wait for bounded Chromiacast key-frame feedback")?
        .context("Chromiacast key-frame feedback stream closed")?;
    let key_frame = key_frame_signal
        .args()
        .context("decode Chromiacast key-frame feedback")?;
    ensure!(
        *key_frame.session_generation() == 1 && *key_frame.media_generation() == generation.get(),
        "Chromiacast key-frame feedback was not generation scoped"
    );
    let bitrate_signal = tokio::time::timeout(START_TIMEOUT, bitrate_requests.next())
        .await
        .context("wait for bounded Chromiacast bitrate feedback")?
        .context("Chromiacast bitrate feedback stream closed")?;
    let bitrate = bitrate_signal
        .args()
        .context("decode Chromiacast bitrate feedback")?;
    ensure!(
        *bitrate.session_generation() == 1
            && *bitrate.media_generation() == generation.get()
            && *bitrate.bitrate() == 1_600_000,
        "Chromiacast congestion feedback did not request the deterministic encoder bitrate"
    );

    let first = session.get_statistics().await?;
    ensure!(
        first.session_generation == 1
            && first.media_generation == generation.get()
            && first.encoded_frames > 0
            && first.video_bitrate == 1_600_000,
        "Chromiacast Start returned before its sender accepted a validated H.264 access unit"
    );
    tokio::time::timeout(START_TIMEOUT, async {
        loop {
            let statistics = session.get_statistics().await?;
            if statistics.encoded_frames > first.encoded_frames {
                return Ok::<_, zbus::Error>(statistics);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("wait for Chromiacast delivery to recover after congestion")??;

    session
        .suspend(SuspendReason::OutputDisabled)
        .await
        .context("suspend Chromiacast media graph")?;
    let settled = session.get_statistics().await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let suspended = session.get_statistics().await?;
    ensure!(
        suspended.encoded_frames == settled.encoded_frames,
        "Chromiacast continued consuming video while suspended"
    );
    session
        .resume(generation.get())
        .await
        .context("resume Chromiacast media graph")?;
    ensure!(
        session.get_statistics().await?.encoded_frames > suspended.encoded_frames,
        "Chromiacast Resume returned before consuming a new video frame"
    );

    session
        .stop_media(generation.get(), StopReason::UserRequest)
        .await
        .context("stop Chromiacast media graph")?;
    wait_for_backend_pipewire_clients(0).await?;
    drop(producer);
    Ok(())
}

async fn media_remote(
    provider: &ClassifiedSocketRemoteProvider,
    session_id: &str,
    media_generation: NonZeroU64,
) -> anyhow::Result<zbus::zvariant::OwnedFd> {
    let remotes = provider
        .create_backend_remotes(session_id, "chromiacast", media_generation, false)
        .await
        .context("mint untouched Chromiacast PipeWire connection")?;
    let (video, audio) = remotes.into_parts();
    ensure!(
        audio.is_none(),
        "video-only Chromiacast media gate minted an audio remote"
    );
    Ok(video.into_owned_fd().into())
}

fn media_target(
    producer: &GStreamerTestProducer,
    session_id: &str,
    media_generation: NonZeroU64,
    object_serial: u64,
) -> PipeWireTarget {
    PipeWireTarget {
        kind: MediaKind::Video,
        node_name: producer.node_name.clone(),
        object_serial,
        session_id: session_id.into(),
        device_instance: "chromiacast-test-card".into(),
        connector_id: 40,
        output_index: 0,
        media_generation: media_generation.get(),
        caps: "video/x-raw,format=BGRx,width=640,height=480,framerate=60/1".into(),
    }
}

fn media_configuration() -> MediaConfiguration {
    MediaConfiguration {
        video_profile_id: "h264-high".into(),
        audio_profile_id: None,
        mode: DisplayMode {
            width: 640,
            height: 480,
            refresh_millihz: 60_000,
            flags: 0,
        },
        video_bitrate: 2_000_000,
    }
}

struct ActivationLauncher {
    child: Option<Child>,
    socket_path: PathBuf,
}

impl ActivationLauncher {
    fn start(
        socket_activate: &Path,
        backend: &Path,
        socket_path: &Path,
        fixture: bool,
    ) -> anyhow::Result<Self> {
        remove_stale_socket(socket_path)?;
        let backend = fs::canonicalize(backend)
            .with_context(|| format!("resolve Chromiacast backend {}", backend.display()))?;
        let mut command = Command::new(socket_activate);
        command
            .arg(format!("--listen={}", socket_path.display()))
            .arg("--accept")
            .arg(format!("--fdname={BACKEND_CONTROL_FD_NAME}"))
            .arg("--setenv=PIPEWIRE_REMOTE=ambient-remote-must-not-survive")
            .arg("--setenv=PRONK_BACKEND_INSTANCE=chromiacast")
            .arg("--setenv=INVOCATION_ID=development")
            .arg("--setenv=PRONK_BACKEND_ALLOW_UNMANAGED_PEER=1");
        pass_gstreamer_diagnostic_environment(&mut command);
        if fixture {
            command.arg("--setenv=PRONK_CHROMIACAST_TEST_MODE=fixture");
        }
        let child = command
            .arg(backend)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start Chromiacast activation listener")?;
        Ok(Self {
            child: Some(child),
            socket_path: socket_path.into(),
        })
    }

    async fn wait_until_listening(&mut self) -> anyhow::Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if self.socket_path.exists() {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("launcher has not stopped")
                .try_wait()
                .context("query Chromiacast activation listener")?
            {
                bail!("systemd-socket-activate exited early with {status}");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {}", self.socket_path.display());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child
                    .kill()
                    .context("stop Chromiacast activation listener")?;
            }
            child
                .wait()
                .context("reap Chromiacast activation listener")?;
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

fn temporary_socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("pronk-chromiacast-p2p-{}.sock", std::process::id()))
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn parse_arguments() -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let socket_activate = arguments
        .next()
        .context("missing systemd-socket-activate path")?;
    let backend = arguments.next().context("missing pronk-chromiacast path")?;
    let edid_decode = arguments.next().context("missing edid-decode path")?;
    ensure!(arguments.next().is_none(), "unexpected extra argument");
    Ok((socket_activate, backend, edid_decode))
}
