use std::num::NonZeroU64;
use std::sync::Arc;

use futures_util::StreamExt;
use pronk_backend_protocol::{
    AudioProfile, DeviceCapabilities, DisplayIdentity, DisplayMode, IdentitySource, VideoProfile,
    SESSION_FEATURE_AUDIO,
};
use pronk_core::identity::{PnpIdResolver, DEFAULT_SYNTHESIZER_PNP_ID};
use pronk_core::output::{CastKmsOutput, CastKmsOutputId, OutputConnection};
use pronk_dbus::{
    CastDisplay1Proxy, DeviceAvailability, Manager1Proxy, MediaSession1Proxy, MediaSessionPhase,
};
use tokio::net::UnixStream;
use zbus::connection::{AuthMechanism, Builder};
use zbus::Guid;

use super::*;
use crate::display_state::{ActiveRoute, DisplayTopology, MediaStatus, RouteTarget, RoutedMode};
use crate::manager::ManagerActor;
use crate::preparation::PreparedCastDevice;
use crate::test_support::UnreachableKernelSessionProvider;

fn device() -> DeviceInfo {
    DeviceInfo {
        backend_id: "mock".into(),
        device_id: "living-room".into(),
        display_name: "Living Room TV".into(),
        availability: DeviceAvailability::Available,
        connection_generation: 1,
        discovery_generation: 2,
        device_revision: 1,
        metadata: Vec::new(),
    }
}

fn added_display_snapshot(display_id: CastDisplayId) -> crate::display::AddedCastDisplaySnapshot {
    added_display_snapshot_with_audio(display_id, false)
}

fn added_display_snapshot_with_audio(
    display_id: CastDisplayId,
    audio_enabled: bool,
) -> crate::display::AddedCastDisplaySnapshot {
    let device = device();
    let resolver =
        PnpIdResolver::from_database("GGL\tGoogle Inc.\n", &[], DEFAULT_SYNTHESIZER_PNP_ID)
            .unwrap();
    let prepared = PreparedCastDevice::from_capabilities(
        device.clone(),
        DeviceCapabilities {
            preparation_generation: 1,
            display_identity: DisplayIdentity {
                manufacturer_name: Some("Google".into()),
                manufacturer_source: IdentitySource::AuthenticatedDeviceInfo,
                product_name: Some("Mock Display".into()),
                product_source: IdentitySource::SetupEndpoint,
                pnp_id: None,
            },
            modes: vec![DisplayMode {
                width: 640,
                height: 480,
                refresh_millihz: 60_000,
                flags: 0,
            }],
            video_profiles: vec![VideoProfile {
                profile_id: "h264-high".into(),
                codec: "h264".into(),
                max_width: 1920,
                max_height: 1080,
                max_refresh_millihz: 60_000,
                raw_layouts: vec![pronk_backend_protocol::RawVideoLayout::system_memory(
                    u32::from_le_bytes(*b"XR24"),
                )],
            }],
            audio_profiles: if audio_enabled {
                vec![AudioProfile {
                    profile_id: "opus-stereo".into(),
                    codec: "opus".into(),
                    max_channels: 2,
                    sample_rates: vec![48_000],
                }]
            } else {
                Vec::new()
            },
            features: if audio_enabled {
                SESSION_FEATURE_AUDIO
            } else {
                0
            },
        },
        &resolver,
        audio_enabled,
    )
    .unwrap();
    let state_revision = device.device_revision;
    crate::display::AddedCastDisplaySnapshot {
        display_id,
        state_revision,
        device,
        prepared,
        output: CastKmsOutput {
            id: CastKmsOutputId {
                device_path: "/sys/devices/virtual/castkms".into(),
                output_index: 0,
            },
            node_path: "/dev/dri/card42".into(),
            device_major: 226,
            device_minor: 42,
            crtc_id: 57,
            connector_id: 77,
            connector_name: "Virtual-1".into(),
            connection: OutputConnection::Connected,
        },
        kernel_session_id: NonZeroU64::new(9).unwrap(),
        grant_state: crate::display_state::DisplayGrantState::Active,
        runtime: crate::display_state::DisplayRuntimeState::attached(state_revision),
    }
}

#[test]
fn public_media_projection_coalesces_internal_phases() {
    let display_id = CastDisplayId::generate().unwrap();
    let mut snapshot = added_display_snapshot_with_audio(display_id, true);
    let cases = [
        (MediaStatus::Idle, MediaSessionPhase::Inactive, 0),
        (MediaStatus::StartingCapture, MediaSessionPhase::Starting, 1),
        (MediaStatus::StartingMedia, MediaSessionPhase::Starting, 1),
        (MediaStatus::Running, MediaSessionPhase::Running, 1),
        (MediaStatus::Suspended, MediaSessionPhase::Suspended, 1),
        (MediaStatus::Reconfiguring, MediaSessionPhase::Recovering, 1),
        (MediaStatus::Stopping, MediaSessionPhase::Stopping, 1),
        (
            MediaStatus::Failed("transport failed".into()),
            MediaSessionPhase::Failed,
            1,
        ),
    ];
    for (internal, public, generation) in cases {
        snapshot.runtime.observe_media(generation, internal);
        snapshot.state_revision = snapshot.runtime.revision();
        let projected = public_media_session_state(&snapshot);
        projected.validate().unwrap();
        assert_eq!(projected.phase, public);
        assert_eq!(projected.media_generation, generation);
        assert!(projected.audio_enabled);
    }

    snapshot.runtime.observe_media(
        1,
        MediaStatus::Failed(format!(
            "\n{}é",
            "x".repeat(pronk_dbus::MAX_MEDIA_ERROR_BYTES)
        )),
    );
    snapshot.state_revision = snapshot.runtime.revision();
    let bounded = public_media_session_state(&snapshot);
    bounded.validate().unwrap();
    assert_eq!(bounded.error.len(), pronk_dbus::MAX_MEDIA_ERROR_BYTES);
    assert!(!bounded.error.chars().any(char::is_control));

    snapshot
        .runtime
        .observe_media(1, MediaStatus::Failed("\n\t".into()));
    snapshot.state_revision = snapshot.runtime.revision();
    let missing = public_media_session_state(&snapshot);
    missing.validate().unwrap();
    assert_eq!(
        missing.error,
        "media session failed without diagnostic detail"
    );
}

#[tokio::test]
async fn public_interface_lists_and_emits_revisioned_devices() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let actor =
        ManagerActor::spawn(Vec::new(), Arc::new(UnreachableKernelSessionProvider)).unwrap();
    let server = Builder::unix_stream(server_stream)
        .server(Guid::generate())
        .unwrap()
        .p2p()
        .auth_mechanism(AuthMechanism::External)
        .serve_at(MANAGER_PATH, ManagerInterface::new(actor.handle()))
        .unwrap();
    let client = Builder::unix_stream(client_stream)
        .p2p()
        .auth_mechanism(AuthMechanism::External);
    let (server_connection, client_connection) =
        tokio::try_join!(server.build(), client.build()).unwrap();
    let proxy = Manager1Proxy::new(&client_connection).await.unwrap();

    assert_eq!(proxy.get_version().await.unwrap(), ApiVersion::CURRENT);
    assert_eq!(
        proxy.list_devices().await.unwrap(),
        DeviceSnapshot {
            inventory_revision: 0,
            devices: Vec::new(),
        }
    );
    assert_eq!(
        proxy.list_displays().await.unwrap(),
        CastDisplaySnapshot {
            displays: Vec::new(),
        }
    );

    let mut added = proxy.receive_device_added().await.unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    let signal_task =
        tokio::spawn(async move { emit_inventory_events(&server_connection, event_rx).await });
    event_tx
        .send(InventoryEvent::DeviceAdded {
            inventory_revision: 1,
            device: device(),
        })
        .await
        .unwrap();
    let signal = added.next().await.unwrap();
    let args = signal.args().unwrap();
    assert_eq!(*args.inventory_revision(), 1);
    assert_eq!(args.device(), &device());

    drop(event_tx);
    signal_task.await.unwrap().unwrap();
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn cast_display_object_returns_bounded_info_and_removes_idempotently() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let actor =
        ManagerActor::spawn(Vec::new(), Arc::new(UnreachableKernelSessionProvider)).unwrap();
    let display_id = CastDisplayId::generate().unwrap();
    let path = display_path(display_id).unwrap();
    let info = CastDisplayInfo {
        display_id: display_id.to_string(),
        backend_id: "mock".into(),
        device_id: "living-room".into(),
        display_name: "Living Room TV".into(),
        manufacturer_name: "Google".into(),
        manufacturer_source: DisplayIdentitySource::AuthenticatedDeviceInfo,
        product_name: "Mock Display".into(),
        product_source: DisplayIdentitySource::SetupEndpoint,
        pnp_id: "GGL".into(),
        pnp_resolution_source: PnpResolutionSource::LegalSuffixName,
        connector_id: 77,
        connector_name: "Virtual-1".into(),
        output_index: 0,
        product_code: 42,
        serial: 99,
        attachment_state: DisplayAttachmentState::Attached,
    };
    info.validate().unwrap();
    let state = CastDisplayState {
        revision: 1,
        device: device(),
        attachment_state: DisplayAttachmentState::Attached,
        route_state: DisplayRouteState::Disabled,
        routed_mode: None,
    };
    state.validate().unwrap();
    let server = Builder::unix_stream(server_stream)
        .server(Guid::generate())
        .unwrap()
        .p2p()
        .auth_mechanism(AuthMechanism::External)
        .serve_at(
            path.clone(),
            CastDisplayInterface::new(actor.handle(), display_id, info.clone(), state.clone()),
        )
        .unwrap();
    let client = Builder::unix_stream(client_stream)
        .p2p()
        .auth_mechanism(AuthMechanism::External);
    let (_server_connection, client_connection) =
        tokio::try_join!(server.build(), client.build()).unwrap();
    let proxy = CastDisplay1Proxy::builder(&client_connection)
        .path(path)
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(proxy.get_info().await.unwrap(), info);
    assert_eq!(proxy.get_state().await.unwrap(), state);
    proxy.remove().await.unwrap();
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_events_register_signal_and_remove_cast_display_objects() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let actor =
        ManagerActor::spawn(Vec::new(), Arc::new(UnreachableKernelSessionProvider)).unwrap();
    let server = Builder::unix_stream(server_stream)
        .server(Guid::generate())
        .unwrap()
        .p2p()
        .auth_mechanism(AuthMechanism::External)
        .serve_at(MANAGER_PATH, ManagerInterface::new(actor.handle()))
        .unwrap();
    let client = Builder::unix_stream(client_stream)
        .p2p()
        .auth_mechanism(AuthMechanism::External);
    let (server_connection, client_connection) =
        tokio::try_join!(server.build(), client.build()).unwrap();
    let manager_proxy = Manager1Proxy::new(&client_connection).await.unwrap();
    let mut added_signals = manager_proxy.receive_display_added().await.unwrap();
    let mut removed_signals = manager_proxy.receive_display_removed().await.unwrap();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let lifecycle_manager = actor.handle();
    let lifecycle_task = tokio::spawn(async move {
        serve_lifecycle_events(&server_connection, lifecycle_manager, event_rx).await
    });
    let display_id = CastDisplayId::generate().unwrap();
    let snapshot = added_display_snapshot(display_id);
    let expected = public_display(&snapshot);
    let expected_state = public_display_state(&snapshot);
    event_tx
        .send(LifecycleEvent::Added(Box::new(snapshot)))
        .unwrap();

    let added = added_signals.next().await.unwrap();
    assert_eq!(added.args().unwrap().display(), &expected);
    let path = display_path(display_id).unwrap();
    let display_proxy = CastDisplay1Proxy::builder(&client_connection)
        .path(path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let media_proxy = MediaSession1Proxy::builder(&client_connection)
        .path(path.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut object_removed = display_proxy.receive_removed().await.unwrap();
    assert_eq!(display_proxy.get_info().await.unwrap(), expected);
    assert_eq!(display_proxy.get_state().await.unwrap(), expected_state);
    let initial_media = media_proxy.get_state().await.unwrap();
    initial_media.validate().unwrap();
    assert_eq!(initial_media.phase, MediaSessionPhase::Inactive);
    assert_eq!(initial_media.media_generation, 0);
    assert!(!initial_media.audio_enabled);

    let mut state_changes = display_proxy.receive_state_changed().await.unwrap();
    let mut media_changes = media_proxy.receive_state_changed().await.unwrap();
    let mut changed_snapshot = added_display_snapshot(display_id);
    changed_snapshot.device.display_name = "Living Room TV renamed".into();
    changed_snapshot.device.availability = DeviceAvailability::Unavailable;
    changed_snapshot.device.device_revision = 2;
    changed_snapshot
        .runtime
        .observe_topology(DisplayTopology::Attached {
            route: Some(ActiveRoute {
                target: RouteTarget::new(std::num::NonZeroU32::new(19).unwrap()),
                mode: RoutedMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    flags: 0,
                },
            }),
        });
    changed_snapshot.state_revision = changed_snapshot.runtime.revision();
    let changed_info = public_display(&changed_snapshot);
    let changed_state = public_display_state(&changed_snapshot);
    event_tx
        .send(LifecycleEvent::StateChanged(Box::new(
            changed_snapshot.clone(),
        )))
        .unwrap();
    let changed = state_changes.next().await.unwrap();
    assert_eq!(changed.args().unwrap().state(), &changed_state);
    assert_eq!(changed_state.route_state, DisplayRouteState::Active);
    assert_eq!(changed_state.routed_mode.unwrap().width, 1920);
    assert_eq!(display_proxy.get_state().await.unwrap(), changed_state);
    assert_eq!(display_proxy.get_info().await.unwrap(), changed_info);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), media_changes.next())
            .await
            .is_err()
    );
    assert_eq!(media_proxy.get_state().await.unwrap(), initial_media);

    changed_snapshot
        .runtime
        .observe_media(1, MediaStatus::Running);
    changed_snapshot.state_revision = changed_snapshot.runtime.revision();
    let expected_running = public_media_session_state(&changed_snapshot);
    event_tx
        .send(LifecycleEvent::StateChanged(Box::new(changed_snapshot)))
        .unwrap();
    let display_media_change = state_changes.next().await.unwrap();
    assert_eq!(
        display_media_change.args().unwrap().state().revision,
        expected_running.revision
    );
    let media_change = media_changes.next().await.unwrap();
    assert_eq!(media_change.args().unwrap().state(), &expected_running);
    assert_eq!(expected_running.phase, MediaSessionPhase::Running);
    assert_eq!(expected_running.media_generation, 1);
    assert_eq!(media_proxy.get_state().await.unwrap(), expected_running);

    event_tx
        .send(LifecycleEvent::Removed { display_id })
        .unwrap();
    object_removed.next().await.unwrap();
    let removed = removed_signals.next().await.unwrap();
    assert_eq!(
        removed.args().unwrap().display_id(),
        &display_id.to_string()
    );
    assert!(display_proxy.get_info().await.is_err());
    assert!(media_proxy.get_state().await.is_err());

    drop(event_tx);
    lifecycle_task.await.unwrap().unwrap();
    actor.shutdown().await.unwrap();
}
