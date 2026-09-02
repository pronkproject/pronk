//! Opt-in live coverage for compositor-issued or administratively launched
//! CastKMS grants and media transport.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use castkms_sys::{
    dma_buf_ioctl_sync, drm_ioctl_castkms_get_grant, DmaBufSync, DrmCastkmsGetGrant,
    CAPTURE_FRAME_FULL_DAMAGE, CAPTURE_FRAME_MODE_CHANGED, CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR,
    DISPLAY_CEC_AUDIO_V1_RIGHTS, DISPLAY_CEC_V1_RIGHTS, DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ,
    DMA_BUF_SYNC_START, DMA_BUF_SYNC_WRITE, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888,
    GRANT_FLAG_ADMIN,
};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::libc;
use nix::sys::stat::{major, minor};
use pronk::mutter_grant_provider::MutterGrantProvider;
use pronk_core::castkms::{
    AsyncCastKmsClient, CaptureBufferInfo, CaptureBufferState, CaptureError, CaptureFrameEvent,
    CaptureQueue, CaptureStopOutcome, CaptureStreamInfo, CaptureSynchronization, CastKmsClient,
    CastKmsError, CastKmsEvent, CursorCaptureMode, GrantCaptureReconciliation, GrantState,
    GrantStateEvidence, ValidatedEdid,
};
use pronk_core::grant::{GrantLease, GrantMetadata, GrantProfile, GrantProvider, GrantTarget};
use pronk_dbus::BUS_NAME;
use pronk_pipewire::{
    ClassifiedSocketPaths, ClassifiedSocketRemoteProvider, PipeWireBufferTransport, PipeWireRemote,
    VideoBuffer, VideoBufferLayout, VideoDamage, VideoFrame, VideoNodeIdentity, VideoSourceActor,
    VideoSourceActorEvent, VideoSourceConfig, VideoSourceGeneration, VideoSyncTimelines,
};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

const SYSFS_TIMEOUT: Duration = Duration::from_secs(2);
const GRANT_ACTIVE_TIMEOUT: Duration = Duration::from_secs(10);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
const MODE_CHANGE_TIMEOUT: Duration = Duration::from_secs(60);
const GRANT_SUSPENSION_TIMEOUT: Duration = Duration::from_secs(60);
const CAPTURE_USER_DATA: u64 = 0x5052_4f4e_4b00_0001;
const CAPTURE_POOL_SIZE: usize = 4;
const PIPEWIRE_FRAME_COUNT: usize = 30;
const PIPEWIRE_MODE_GENERATION_FRAME_COUNT: usize = PIPEWIRE_FRAME_COUNT / 2;
const CAPTURE_SENTINEL: u8 = 0x77;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let device = PathBuf::from(arguments.next().context("missing DRM device")?);
    let login_session_id = arguments
        .next()
        .context("missing login session ID")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("login session ID is not UTF-8"))?;
    let connector_id = arguments
        .next()
        .context("missing connector ID")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("connector ID is not UTF-8"))?
        .parse::<u32>()
        .context("parse connector ID")?;
    let crtc_id = arguments
        .next()
        .context("missing CRTC ID")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("CRTC ID is not UTF-8"))?
        .parse::<u32>()
        .context("parse CRTC ID")?;
    let crtc_id = NonZeroU32::new(crtc_id).context("CRTC ID is zero")?;
    ensure!(
        arguments.next().is_none(),
        "expected DEVICE SESSION-ID CONNECTOR-ID CRTC-ID"
    );
    let exercise_mode_restart = match std::env::var_os("PRONK_VM_MODE_CHANGE_GATE") {
        None => false,
        Some(value) if value.to_str() == Some("signal") => true,
        Some(_) => bail!("PRONK_VM_MODE_CHANGE_GATE must be unset or 'signal'"),
    };
    let exercise_grant_suspension = match std::env::var_os("PRONK_VM_GRANT_SUSPENSION_GATE") {
        None => false,
        Some(value) if value.to_str() == Some("output-disable") => true,
        Some(_) => {
            bail!("PRONK_VM_GRANT_SUSPENSION_GATE must be unset or 'output-disable'")
        }
    };
    let exercise_master_reopen = match std::env::var_os("PRONK_VM_MASTER_REOPEN_GATE") {
        None => false,
        Some(value) if value.to_str() == Some("detach-reattach") => true,
        Some(_) => bail!("PRONK_VM_MASTER_REOPEN_GATE must be unset or 'detach-reattach'"),
    };
    let exercise_grant_owner_handoff = match std::env::var_os("PRONK_VM_GRANT_OWNER_HANDOFF_GATE") {
        None => false,
        Some(value) if value.to_str() == Some("release-name") => true,
        Some(_) => {
            bail!("PRONK_VM_GRANT_OWNER_HANDOFF_GATE must be unset or 'release-name'")
        }
    };
    let inherited_administrative_grant = match std::env::var_os("PRONK_VM_INHERITED_ADMIN_GRANT") {
        None => false,
        Some(value) if value.to_str() == Some("external-control") => true,
        Some(_) => {
            bail!("PRONK_VM_INHERITED_ADMIN_GRANT must be unset or 'external-control'")
        }
    };
    let pipewire_remote = match std::env::var_os("PRONK_VM_PIPEWIRE_GATE") {
        None => None,
        Some(value) if value.to_str() == Some("ambient-development") => {
            Some(PipeWireGateRemote::AmbientDevelopment)
        }
        Some(value) if value.to_str() == Some("classified-core") => {
            Some(PipeWireGateRemote::ClassifiedCore)
        }
        Some(_) => bail!(
            "PRONK_VM_PIPEWIRE_GATE must be unset, 'classified-core', or 'ambient-development'"
        ),
    };
    let pipewire_mode_restart_remote =
        match std::env::var_os("PRONK_VM_PIPEWIRE_MODE_CHANGE_GATE") {
            None => None,
            Some(value) if value.to_str() == Some("signal-classified-core") => {
                Some(PipeWireGateRemote::ClassifiedCore)
            }
            Some(value) if value.to_str() == Some("signal-ambient-development") => {
                Some(PipeWireGateRemote::AmbientDevelopment)
            }
            Some(_) => bail!(
                "PRONK_VM_PIPEWIRE_MODE_CHANGE_GATE must be unset, 'signal-classified-core', or 'signal-ambient-development'"
            ),
        };
    ensure!(
        usize::from(exercise_mode_restart)
            + usize::from(exercise_grant_suspension)
            + usize::from(exercise_master_reopen)
            + usize::from(exercise_grant_owner_handoff)
            + usize::from(pipewire_remote.is_some())
            + usize::from(pipewire_mode_restart_remote.is_some())
            <= 1,
        "enable at most one orchestrated VM gate per invocation"
    );
    ensure!(
        !inherited_administrative_grant || !exercise_grant_owner_handoff,
        "the grant-owner handoff gate requires compositor-issued grants"
    );

    let metadata = std::fs::metadata(&device)
        .with_context(|| format!("inspect DRM device {}", device.display()))?;
    let target = GrantTarget {
        device_major: u32::try_from(major(metadata.rdev())).context("device major is too large")?,
        device_minor: u32::try_from(minor(metadata.rdev())).context("device minor is too large")?,
        connector_id,
        profile: GrantProfile::DisplayCecV1,
    };

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?;
    let (connection, lease) = if inherited_administrative_grant {
        (
            None,
            inherited_administrative_lease(connector_id)
                .context("adopt externally controlled administrative grant")?,
        )
    } else {
        let connection = runtime
            .block_on(zbus::Connection::session())
            .context("connect to the graphical session bus")?;
        runtime
            .block_on(connection.request_name(BUS_NAME))
            .context("own the Pronk bus name used by Mutter authorization")?;
        let lease = runtime
            .block_on(
                MutterGrantProvider::new(connection.clone())
                    .acquire(target.clone(), CancellationToken::new()),
            )
            .context("acquire CastKMS grant from Mutter")?;
        (Some(connection), lease)
    };
    ensure!(lease.grant_id() != 0, "grant has a zero ID");
    ensure!(lease.connector_id() == connector_id, "connector ID differs");
    ensure!(
        lease.rights()
            == if inherited_administrative_grant {
                DISPLAY_CEC_AUDIO_V1_RIGHTS
            } else {
                DISPLAY_CEC_V1_RIGHTS
            },
        "rights differ"
    );
    ensure!(
        lease.flags()
            == if inherited_administrative_grant {
                GRANT_FLAG_ADMIN
            } else {
                0
            },
        "grant flags differ"
    );
    let expected_grant_id = lease.grant_id();

    let client = CastKmsClient::new(lease).context("construct inherited-holder client")?;
    let mut client = {
        let _runtime_guard = runtime.enter();
        client
            .into_async()
            .context("register inherited holder with Tokio")?
    };
    let live = client
        .client()
        .query_grant()
        .context("query grant through client")?;
    ensure!(
        live.grant_id == expected_grant_id,
        "client grant ID differs"
    );
    ensure!(
        live.connector_id == connector_id,
        "client connector differs"
    );
    if exercise_grant_owner_handoff {
        let connection = connection
            .as_ref()
            .context("grant-owner handoff has no compositor connection")?;
        exercise_grant_owner_handoff_gate(
            &runtime,
            connection,
            target,
            &mut client,
            expected_grant_id,
        )?;
        println!("grant_owner_release_revokes=pass");
        println!("grant_owner_replacement_acquires=pass");
        return Ok(());
    }

    let first_edid = diagnostic_edid(1)?;
    let second_edid = diagnostic_edid(2)?;
    client
        .client()
        .attach_monitor(&first_edid, "CastKMS Live Test")
        .context("attach monitor with initial EDID")?;

    let card_name = device
        .file_name()
        .and_then(|name| name.to_str())
        .context("DRM device basename is not UTF-8")?;
    let connector_sysfs = wait_for_connector_edid(card_name, first_edid.as_bytes())?;
    wait_for_status(&connector_sysfs, "connected")?;
    let activation_path = wait_for_active_grant(&runtime, &mut client)?;

    let capabilities = client
        .client()
        .query_capture_capabilities(crtc_id)
        .context("query routed CRTC capture capabilities")?;
    ensure!(
        !capabilities.formats().is_empty(),
        "capture format list is empty"
    );
    ensure!(
        capabilities.max_registered_buffers() >= CAPTURE_POOL_SIZE as u32,
        "capture buffer limit is smaller than {CAPTURE_POOL_SIZE}"
    );
    let stream = client
        .client_mut()
        .start_capture(&capabilities, CursorCaptureMode::IncludeInFrame)
        .context("start routed CRTC capture")?;
    ensure!(stream.crtc_id == crtc_id, "capture stream CRTC differs");
    ensure!(stream.width.get() > 0, "capture stream width is zero");
    ensure!(stream.height.get() > 0, "capture stream height is zero");
    ensure!(stream.refresh_hz.get() > 0, "capture refresh rate is zero");

    let mut buffers = Vec::with_capacity(CAPTURE_POOL_SIZE);
    for index in 0..CAPTURE_POOL_SIZE {
        let buffer = client
            .client_mut()
            .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Implicit)
            .with_context(|| format!("allocate and register capture buffer {index}"))?;
        ensure!(
            buffer.stream_id == stream.stream_id,
            "buffer stream differs"
        );
        ensure!(
            buffer.state == CaptureBufferState::Idle,
            "buffer is not idle"
        );
        ensure!(
            !buffers
                .iter()
                .any(|existing: &pronk_core::castkms::CaptureBufferInfo| {
                    existing.buffer_id == buffer.buffer_id
                        || existing.framebuffer_id == buffer.framebuffer_id
                }),
            "capture pool contains a duplicate identifier"
        );
        let layout = buffer.layout.context("owned buffer has no layout")?;
        ensure!(layout.width == stream.width, "buffer width differs");
        ensure!(layout.height == stream.height, "buffer height differs");
        ensure!(
            layout.format == DRM_FORMAT_XRGB8888,
            "buffer format differs"
        );
        ensure!(
            layout.modifier == DRM_FORMAT_MOD_LINEAR,
            "buffer modifier differs"
        );
        ensure!(
            layout.pitch.get() >= layout.width.get() * 4,
            "buffer pitch is too small"
        );
        buffers.push(buffer);
    }
    let buffer = buffers[0];
    let layout = buffer
        .layout
        .expect("pool validation required an owned buffer layout");
    let sentinel_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(stream.stream_id, buffer.buffer_id)
            .context("borrow owned capture DMA-BUF")?;
        let descriptor_flags = fcntl(dma_buf.as_raw_fd(), FcntlArg::F_GETFD)
            .context("inspect capture DMA-BUF flags")?;
        ensure!(
            FdFlag::from_bits_truncate(descriptor_flags).contains(FdFlag::FD_CLOEXEC),
            "capture DMA-BUF lacks CLOEXEC"
        );
        write_dma_buf_pattern(dma_buf, layout.size, CAPTURE_SENTINEL)
            .context("prefill capture DMA-BUF")?
    };

    let user_data = NonZeroU64::new(CAPTURE_USER_DATA).expect("test user data is nonzero");
    let queue = client
        .client_mut()
        .queue_capture_buffer(buffer.buffer_id, user_data)
        .context("queue implicit capture buffer")?;
    ensure!(queue.stream_id == stream.stream_id, "queue stream differs");
    ensure!(
        queue.ready_point.is_none(),
        "implicit queue has ready point"
    );
    ensure!(
        queue.reuse_point.is_none(),
        "implicit queue has reuse point"
    );
    let fence = client
        .client()
        .export_implicit_capture_fence(stream.stream_id, buffer.buffer_id)
        .context("export queued capture producer fence")?;
    ensure!(
        fence.stream_id() == stream.stream_id,
        "fence stream differs"
    );
    ensure!(
        fence.buffer_id() == buffer.buffer_id,
        "fence buffer differs"
    );
    ensure!(fence.user_data() == user_data, "fence user data differs");

    let event = wait_for_capture_frame(&runtime, &mut client, queue)?;
    validate_capture_frame(
        event,
        stream.mode_generation.get(),
        stream.width.get(),
        stream.height.get(),
        queue,
    )?;
    let ready = runtime
        .block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, fence.wait()).await })
        .context("timed out waiting for implicit capture producer fence")?
        .context("wait for implicit capture producer fence")?;
    let completion = client
        .client_mut()
        .take_capture_completion(ready)
        .context("take synchronized capture completion")?;
    ensure!(completion.queue == queue, "completion queue differs");
    ensure!(completion.frame == event, "completion event differs");
    let captured_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(stream.stream_id, buffer.buffer_id)
            .context("borrow completed capture DMA-BUF")?;
        hash_dma_buf(dma_buf, layout.size).context("hash completed capture DMA-BUF")?
    };
    ensure!(
        captured_hash != sentinel_hash,
        "capture left the sentinel pixels unchanged"
    );
    let implicit_release = client
        .client_mut()
        .release_capture_buffer(stream.stream_id, buffer.buffer_id)
        .context("release implicit capture buffer")?;
    ensure!(
        implicit_release.reuse_point.is_none(),
        "implicit release has a reuse point"
    );
    for buffer in &buffers {
        let unregistered = client
            .client_mut()
            .unregister_capture_buffer(buffer.buffer_id)
            .with_context(|| {
                format!("unregister and destroy capture buffer {}", buffer.buffer_id)
            })?;
        ensure!(
            unregistered.state == CaptureBufferState::Idle,
            "unregistered buffer was not idle"
        );
    }
    ensure!(
        client.client().capture_buffers(stream.stream_id).is_empty(),
        "capture buffer remained tracked after unregister"
    );

    let explicit_buffer = client
        .client_mut()
        .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Explicit)
        .context("allocate and register explicit capture buffer")?;
    ensure!(
        explicit_buffer.synchronization == CaptureSynchronization::Explicit,
        "explicit buffer synchronization differs"
    );
    let explicit_layout = explicit_buffer
        .layout
        .context("explicit owned buffer has no layout")?;
    let first_explicit_sentinel_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(stream.stream_id, explicit_buffer.buffer_id)
            .context("borrow first-use explicit DMA-BUF")?;
        write_dma_buf_pattern(dma_buf, explicit_layout.size, 0x55)
            .context("prefill first-use explicit DMA-BUF")?
    };
    let first_explicit_user_data =
        NonZeroU64::new(CAPTURE_USER_DATA + 1).expect("explicit user data is nonzero");
    let first_explicit_queue = client
        .client_mut()
        .queue_capture_buffer(explicit_buffer.buffer_id, first_explicit_user_data)
        .context("queue first-use explicit capture buffer")?;
    ensure!(
        first_explicit_queue.ready_point == NonZeroU64::new(1),
        "first explicit ready point differs"
    );
    ensure!(
        first_explicit_queue.reuse_point.is_none(),
        "first explicit queue has a reuse point"
    );
    let first_explicit_fence = client
        .client()
        .arm_explicit_capture_fence(stream.stream_id, explicit_buffer.buffer_id)
        .context("arm first explicit capture ready point")?;
    ensure!(
        first_explicit_fence.ready_point() == NonZeroU64::new(1).unwrap(),
        "armed first explicit ready point differs"
    );
    let first_explicit_event = wait_for_capture_frame(&runtime, &mut client, first_explicit_queue)?;
    validate_capture_frame(
        first_explicit_event,
        stream.mode_generation.get(),
        stream.width.get(),
        stream.height.get(),
        first_explicit_queue,
    )?;
    let first_explicit_ready = runtime
        .block_on(async {
            tokio::time::timeout(CAPTURE_TIMEOUT, first_explicit_fence.wait()).await
        })
        .context("timed out waiting for first explicit ready point")?
        .context("wait for first explicit ready point")?;
    let first_explicit_completion = client
        .client_mut()
        .take_capture_completion(first_explicit_ready)
        .context("take first explicit capture completion")?;
    ensure!(
        first_explicit_completion.queue == first_explicit_queue,
        "first explicit completion queue differs"
    );
    let first_explicit_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(stream.stream_id, explicit_buffer.buffer_id)
            .context("borrow completed first explicit DMA-BUF")?;
        hash_dma_buf(dma_buf, explicit_layout.size)
            .context("hash completed first explicit DMA-BUF")?
    };
    ensure!(
        first_explicit_hash != first_explicit_sentinel_hash,
        "first explicit capture left the sentinel pixels unchanged"
    );

    let second_explicit_sentinel_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(stream.stream_id, explicit_buffer.buffer_id)
            .context("borrow consumer-owned explicit DMA-BUF for reuse")?;
        write_dma_buf_pattern(dma_buf, explicit_layout.size, 0x66)
            .context("prefill reused explicit DMA-BUF")?
    };
    let first_explicit_release = client
        .client_mut()
        .release_capture_buffer(stream.stream_id, explicit_buffer.buffer_id)
        .context("signal first explicit reuse point")?;
    ensure!(
        first_explicit_release.reuse_point == NonZeroU64::new(1),
        "first explicit reuse point differs"
    );
    let second_explicit_user_data =
        NonZeroU64::new(CAPTURE_USER_DATA + 2).expect("second explicit user data is nonzero");
    let second_explicit_queue = client
        .client_mut()
        .queue_capture_buffer(explicit_buffer.buffer_id, second_explicit_user_data)
        .context("queue reused explicit capture buffer")?;
    ensure!(
        second_explicit_queue.ready_point == NonZeroU64::new(2),
        "second explicit ready point differs"
    );
    ensure!(
        second_explicit_queue.reuse_point == NonZeroU64::new(1),
        "second explicit queue did not depend on reuse point one"
    );
    let second_explicit_fence = client
        .client()
        .arm_explicit_capture_fence(stream.stream_id, explicit_buffer.buffer_id)
        .context("arm second explicit capture ready point")?;
    ensure!(
        second_explicit_fence.ready_point() == NonZeroU64::new(2).unwrap(),
        "armed second explicit ready point differs"
    );
    let second_explicit_event =
        wait_for_capture_frame(&runtime, &mut client, second_explicit_queue)?;
    validate_capture_frame(
        second_explicit_event,
        stream.mode_generation.get(),
        stream.width.get(),
        stream.height.get(),
        second_explicit_queue,
    )?;
    let second_explicit_ready = runtime
        .block_on(async {
            tokio::time::timeout(CAPTURE_TIMEOUT, second_explicit_fence.wait()).await
        })
        .context("timed out waiting for second explicit ready point")?
        .context("wait for second explicit ready point")?;
    let second_explicit_completion = client
        .client_mut()
        .take_capture_completion(second_explicit_ready)
        .context("take second explicit capture completion")?;
    ensure!(
        second_explicit_completion.queue == second_explicit_queue,
        "second explicit completion queue differs"
    );
    let second_explicit_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(stream.stream_id, explicit_buffer.buffer_id)
            .context("borrow completed reused explicit DMA-BUF")?;
        hash_dma_buf(dma_buf, explicit_layout.size)
            .context("hash completed reused explicit DMA-BUF")?
    };
    ensure!(
        second_explicit_hash != second_explicit_sentinel_hash,
        "reused explicit capture left the sentinel pixels unchanged"
    );
    let second_explicit_release = client
        .client_mut()
        .release_capture_buffer(stream.stream_id, explicit_buffer.buffer_id)
        .context("signal second explicit reuse point")?;
    ensure!(
        second_explicit_release.reuse_point == NonZeroU64::new(2),
        "second explicit reuse point differs"
    );
    if !exercise_master_reopen {
        client
            .client_mut()
            .unregister_capture_buffer(explicit_buffer.buffer_id)
            .context("unregister explicit buffer and destroy its syncobjs")?;
        ensure!(
            client.client().capture_buffers(stream.stream_id).is_empty(),
            "explicit capture resources remained tracked"
        );
    }

    let master_reopen = if exercise_master_reopen {
        Some(exercise_master_reopen_gate(
            &runtime,
            &mut client,
            stream,
            crtc_id,
            &first_edid,
            &connector_sysfs,
            explicit_buffer,
        )?)
    } else {
        None
    };
    let pipewire = if let Some(remote) = pipewire_remote {
        Some(exercise_pipewire_gate(
            &runtime,
            &mut client,
            stream,
            &login_session_id,
            remote,
        )?)
    } else {
        None
    };
    let pipewire_mode_restart = if let Some(remote) = pipewire_mode_restart_remote {
        Some(exercise_pipewire_mode_restart_gate(
            &runtime,
            &mut client,
            stream,
            crtc_id,
            expected_grant_id,
            &login_session_id,
            remote,
        )?)
    } else {
        None
    };
    let grant_suspension = if exercise_grant_suspension {
        Some(exercise_grant_suspension_gate(
            &runtime,
            &mut client,
            stream,
            crtc_id,
            expected_grant_id,
        )?)
    } else {
        None
    };
    let mode_restart = if exercise_mode_restart {
        Some(exercise_mode_restart_gate(
            &runtime,
            &mut client,
            stream,
            crtc_id,
            expected_grant_id,
        )?)
    } else if !exercise_grant_suspension
        && !exercise_master_reopen
        && pipewire_mode_restart.is_none()
    {
        let stopped = client
            .client_mut()
            .stop_capture()
            .context("stop routed CRTC capture")?;
        ensure!(
            stopped.stream == stream,
            "stopped capture stream metadata differs"
        );
        ensure!(
            stopped.waiting_buffer_count == 0,
            "bufferless capture stream retained buffers"
        );
        None
    } else {
        None
    };

    client
        .client()
        .set_output_edid(&second_edid)
        .context("replace output EDID")?;
    wait_for_edid(&connector_sysfs, second_edid.as_bytes())?;

    client
        .client()
        .clear_output_edid()
        .context("clear output EDID")?;
    wait_for_edid(&connector_sysfs, &[])?;
    wait_for_status(&connector_sysfs, "connected")?;

    client
        .client()
        .set_output_edid(&second_edid)
        .context("restore output EDID")?;
    wait_for_edid(&connector_sysfs, second_edid.as_bytes())?;
    client.client().detach_monitor().context("detach monitor")?;
    wait_for_status(&connector_sysfs, "disconnected")?;
    wait_for_edid(&connector_sysfs, &[])?;

    ensure!(
        matches!(
            client.client().set_output_edid(&second_edid),
            Err(CastKmsError::SetOutputEdid(Errno::ENOTCONN))
        ),
        "EDID update after detach did not return ENOTCONN"
    );

    client
        .client()
        .attach_monitor(&second_edid, "CastKMS Live Test")
        .context("reattach monitor for lease cleanup test")?;
    wait_for_status(&connector_sysfs, "connected")?;
    wait_for_edid(&connector_sysfs, second_edid.as_bytes())?;
    drop(client);
    wait_for_status(&connector_sysfs, "disconnected")?;
    wait_for_edid(&connector_sysfs, &[])?;
    if let Some(connection) = connection.as_ref() {
        runtime
            .block_on(connection.release_name(BUS_NAME))
            .context("release the Pronk bus name")?;
        println!("mutter_bus_name_authorization=pass");
        println!("mutter_grant_creation=pass");
        println!("normal_grant_validation=pass");
    } else {
        println!("administrative_grant_validation=pass");
        println!("external_grant_control=pass");
    }
    println!("inherited_holder_client=pass");
    println!("authoritative_grant_activation=pass");
    println!("grant_activation_path={activation_path}");
    println!("capture_capability_query=pass");
    println!("four_buffer_capture_pool=pass");
    println!("queued_capture_completion=pass");
    println!("implicit_capture_fence=pass");
    println!("tokio_drm_frame_event=pass");
    println!("capture_pixel_hash={captured_hash:016x}");
    println!("capture_pixel_hash_validation=pass");
    println!("explicit_capture_first_hash={first_explicit_hash:016x}");
    println!("explicit_capture_second_hash={second_explicit_hash:016x}");
    println!("explicit_syncobj_ready_wait=pass");
    println!("explicit_syncobj_reuse=pass");
    println!("explicit_syncobj_cleanup=pass");
    if let Some(mode_restart) = mode_restart {
        println!(
            "mode_change_old_mode={}x{}@generation-{}",
            mode_restart.old_stream.width,
            mode_restart.old_stream.height,
            mode_restart.old_stream.mode_generation
        );
        println!(
            "mode_change_new_mode={}x{}@generation-{}",
            mode_restart.new_stream.width,
            mode_restart.new_stream.height,
            mode_restart.new_stream.mode_generation
        );
        println!(
            "mode_change_replacement_hash={:016x}",
            mode_restart.replacement_hash
        );
        println!("synchronous_estale_detection=pass");
        println!("mode_change_old_pool_cleanup=pass");
        println!("mode_change_same_grant=pass");
        println!("mode_change_replacement_capture=pass");
        println!("mode_change_restart=pass");
    }
    if let Some(grant_suspension) = grant_suspension {
        println!(
            "grant_suspension_state={}",
            grant_state_label(grant_suspension.suspended_state)
        );
        println!(
            "grant_suspension_old_stream={}",
            grant_suspension.old_stream.stream_id
        );
        println!(
            "grant_suspension_new_stream={}",
            grant_suspension.new_stream.stream_id
        );
        println!(
            "grant_suspension_replacement_hash={:016x}",
            grant_suspension.replacement_hash
        );
        println!("grant_state_event_semantics=pass");
        println!("grant_suspension_old_pool_cleanup=pass");
        println!("grant_suspension_same_grant=pass");
        println!("grant_suspension_replacement_capture=pass");
        println!("grant_suspension_restart=pass");
    }
    if let Some(master_reopen) = master_reopen {
        println!(
            "master_reopen_old_stream={}",
            master_reopen.old_stream.stream_id
        );
        println!(
            "master_reopen_new_stream={}",
            master_reopen.new_stream.stream_id
        );
        println!(
            "master_reopen_replacement_hash={:016x}",
            master_reopen.replacement_hash
        );
        println!("master_reopen_no_master=pass");
        println!("master_reopen_foreign_content=pass");
        println!("master_reopen_kernel_stream_invalidated=pass");
        println!("master_reopen_queued_buffer_drain=pass");
        println!("master_reopen_old_pool_cleanup=pass");
        println!("master_reopen_same_grant=pass");
        println!("master_reopen_replacement_capture=pass");
        println!("master_reopen_restart=pass");
    }
    if let Some(pipewire) = pipewire {
        println!("pipewire_node_name={}", pipewire.identity.node_name);
        println!("pipewire_object_id={}", pipewire.identity.object_id);
        println!("pipewire_object_serial={}", pipewire.identity.object_serial);
        println!("pipewire_frames_produced={}", pipewire.frames_produced);
        println!(
            "pipewire_transport={}",
            match pipewire.transport {
                PipeWireBufferTransport::Waited => "waited",
                PipeWireBufferTransport::SyncTimeline => "sync-timeline",
            }
        );
        println!("pipewire_caller_owned_pool=pass");
        println!("pipewire_exact_node_identity=pass");
        println!("pipewire_dma_buf_metadata=pass");
        println!("pipewire_downstream_release=pass");
        println!("pipewire_video_source=pass");
    }
    if let Some(pipewire_mode_restart) = pipewire_mode_restart {
        println!(
            "pipewire_mode_old_mode={}x{}@generation-{}",
            pipewire_mode_restart.old_stream.width,
            pipewire_mode_restart.old_stream.height,
            pipewire_mode_restart.old_stream.mode_generation
        );
        println!(
            "pipewire_mode_new_mode={}x{}@generation-{}",
            pipewire_mode_restart.new_stream.width,
            pipewire_mode_restart.new_stream.height,
            pipewire_mode_restart.new_stream.mode_generation
        );
        println!(
            "pipewire_mode_old_node_name={}",
            pipewire_mode_restart.old_identity.node_name
        );
        println!(
            "pipewire_mode_old_object_serial={}",
            pipewire_mode_restart.old_identity.object_serial
        );
        println!(
            "pipewire_mode_new_node_name={}",
            pipewire_mode_restart.new_identity.node_name
        );
        println!(
            "pipewire_mode_new_object_serial={}",
            pipewire_mode_restart.new_identity.object_serial
        );
        println!(
            "pipewire_mode_old_frames={}",
            pipewire_mode_restart.old_frames
        );
        println!(
            "pipewire_mode_new_frames={}",
            pipewire_mode_restart.new_frames
        );
        println!("pipewire_mode_synchronous_estale=pass");
        println!("pipewire_mode_old_pool_cleanup=pass");
        println!("pipewire_mode_new_pool_cleanup=pass");
        println!("pipewire_mode_object_serial_changed=pass");
        println!("pipewire_mode_actor_recreation=pass");
    }
    println!("exclusive_capture_start_stop=pass");
    println!("validated_edid_attach=pass");
    if inherited_administrative_grant {
        println!("administrative_hotplug_activates_grant=pass");
    } else {
        println!("mutter_hotplug_activates_grant=pass");
    }
    println!("edid_replace_and_clear=pass");
    println!("explicit_monitor_detach=pass");
    println!("lease_drop_detaches_monitor=pass");
    println!("holder_drop_releases_grant=pass");
    Ok(())
}

fn inherited_administrative_lease(connector_id: u32) -> anyhow::Result<GrantLease> {
    let inherited_fd = std::env::var("CASTKMS_GRANT_FD")
        .context("CASTKMS_GRANT_FD is unset for administrative launch")?
        .parse::<i32>()
        .context("CASTKMS_GRANT_FD is not a descriptor number")?;
    ensure!(
        inherited_fd > libc::STDERR_FILENO,
        "CASTKMS_GRANT_FD aliases a standard stream"
    );

    // SAFETY: the administrative launcher transfers ownership of this open
    // descriptor to its child through CASTKMS_GRANT_FD.
    let inherited = unsafe { OwnedFd::from_raw_fd(inherited_fd) };
    let holder_fd = fcntl(
        inherited.as_raw_fd(),
        FcntlArg::F_DUPFD_CLOEXEC(libc::STDERR_FILENO + 1),
    )
    .context("duplicate inherited administrative grant")?;
    // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
    let holder = unsafe { OwnedFd::from_raw_fd(holder_fd) };
    drop(inherited);

    let mut query = DrmCastkmsGetGrant::default();
    // SAFETY: `query` has the checked CastKMS userspace layout and remains
    // writable for the duration of the synchronous ioctl.
    unsafe { drm_ioctl_castkms_get_grant(holder.as_raw_fd(), &mut query) }
        .context("query inherited administrative grant")?;
    ensure!(query.reserved == 0, "grant query reserved field is nonzero");

    GrantLease::from_external_administrator(
        holder,
        GrantMetadata {
            grant_id: query.grant_id,
            connector_id: query.connector_id,
            output_index: query.output_index,
            rights: query.rights,
            flags: query.flags,
            initial_state: query.state,
            capture_uapi_major: CAPTURE_UAPI_MAJOR,
            capture_uapi_minor: CAPTURE_UAPI_MINOR,
        },
        connector_id,
        DISPLAY_CEC_AUDIO_V1_RIGHTS,
    )
    .context("validate inherited administrative grant")
}

fn exercise_grant_owner_handoff_gate(
    runtime: &Runtime,
    old_connection: &zbus::Connection,
    target: GrantTarget,
    old_client: &mut AsyncCastKmsClient,
    old_grant_id: u32,
) -> anyhow::Result<()> {
    runtime
        .block_on(old_connection.release_name(BUS_NAME))
        .context("release the Pronk bus name while retaining its connection")?;

    let replacement_connection = runtime
        .block_on(zbus::Connection::session())
        .context("connect replacement Pronk owner to the graphical session bus")?;
    runtime
        .block_on(replacement_connection.request_name(BUS_NAME))
        .context("transfer the Pronk bus name to the replacement connection")?;
    let replacement_lease = runtime
        .block_on(
            MutterGrantProvider::new(replacement_connection.clone())
                .acquire(target, CancellationToken::new()),
        )
        .context("acquire replacement grant after Pronk owner handoff")?;
    ensure!(
        replacement_lease.grant_id() != old_grant_id,
        "replacement owner received the stale grant ID"
    );

    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + GRANT_ACTIVE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            ensure!(
                !remaining.is_zero(),
                "timed out waiting for the previous owner's grant revocation"
            );
            let events = tokio::time::timeout(remaining, old_client.read_events())
                .await
                .context("timed out reading the previous owner's terminal grant event")?
                .context("read the previous owner's terminal grant event")?;
            for event in events {
                if let CastKmsEvent::GrantRevoked(event) = event {
                    ensure!(event.status == 0, "grant revocation status is nonzero");
                    return Ok(());
                }
            }
        }
    })
}

#[derive(Debug, Clone)]
struct PipeWireGateResult {
    identity: VideoNodeIdentity,
    frames_produced: usize,
    transport: PipeWireBufferTransport,
}

struct PipeWireGateGeneration {
    stream: CaptureStreamInfo,
    identity: VideoNodeIdentity,
    capture_buffers: Vec<CaptureBufferInfo>,
    available: VecDeque<NonZeroU32>,
    transport: Option<PipeWireBufferTransport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeWireGateRemote {
    ClassifiedCore,
    AmbientDevelopment,
}

impl PipeWireGateRemote {
    fn name(self) -> &'static str {
        match self {
            Self::ClassifiedCore => "classified-core",
            Self::AmbientDevelopment => "ambient-development",
        }
    }
}

fn create_pipewire_gate_remote(
    runtime: &Runtime,
    kind: PipeWireGateRemote,
) -> anyhow::Result<PipeWireRemote> {
    let remote = match kind {
        PipeWireGateRemote::AmbientDevelopment => PipeWireRemote::AmbientDevelopment,
        PipeWireGateRemote::ClassifiedCore => {
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                .context("XDG_RUNTIME_DIR is unset for classified PipeWire gate")?;
            let paths = ClassifiedSocketPaths::in_runtime_dir(PathBuf::from(runtime_dir))
                .context("construct classified PipeWire socket paths")?;
            let provider = ClassifiedSocketRemoteProvider::new(paths);
            runtime
                .block_on(provider.create_producer_remote())
                .context("connect classified PipeWire producer endpoint")?
                .into_remote()
        }
    };
    println!("pipewire_producer_remote={}", kind.name());
    Ok(remote)
}

fn exercise_pipewire_gate(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    stream: CaptureStreamInfo,
    login_session_id: &str,
    remote: PipeWireGateRemote,
) -> anyhow::Result<PipeWireGateResult> {
    let mut actor = {
        let _runtime_guard = runtime.enter();
        VideoSourceActor::spawn().context("spawn PipeWire source actor")?
    };
    let mut generation = start_pipewire_gate_generation(
        runtime,
        &actor,
        client,
        stream,
        login_session_id,
        "Pronk CastKMS VM gate",
        remote,
    )?;
    println!(
        "pipewire_source_ready={}:{}:{}",
        generation.identity.node_name,
        generation.identity.object_id,
        generation.identity.object_serial
    );
    std::io::stdout()
        .flush()
        .context("flush PipeWire source readiness marker")?;

    produce_pipewire_frames(
        runtime,
        &mut actor,
        client,
        &mut generation,
        PIPEWIRE_FRAME_COUNT,
        CAPTURE_USER_DATA + 0x100,
    )?;
    let identity = generation.identity.clone();
    let transport = generation
        .transport
        .context("PipeWire generation has no negotiated transport")?;
    stop_pipewire_gate_generation(runtime, &actor, client, &generation, true)?;
    unregister_pipewire_gate_generation(client, &generation)?;
    ensure!(
        runtime
            .block_on(actor.shutdown())
            .context("shut down PipeWire source actor")?
            .is_none(),
        "PipeWire actor retained a generation after explicit stop"
    );

    Ok(PipeWireGateResult {
        identity,
        frames_produced: PIPEWIRE_FRAME_COUNT,
        transport,
    })
}

fn start_pipewire_gate_generation(
    runtime: &Runtime,
    actor: &VideoSourceActor,
    client: &mut AsyncCastKmsClient,
    stream: CaptureStreamInfo,
    login_session_id: &str,
    description: &str,
    remote: PipeWireGateRemote,
) -> anyhow::Result<PipeWireGateGeneration> {
    let mut capture_buffers = Vec::with_capacity(CAPTURE_POOL_SIZE);
    let mut video_buffers = Vec::with_capacity(CAPTURE_POOL_SIZE);
    for index in 0..CAPTURE_POOL_SIZE {
        let capture_buffer = client
            .client_mut()
            .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Explicit)
            .with_context(|| format!("allocate PipeWire capture buffer {index}"))?;
        ensure!(
            capture_buffer.stream_id == stream.stream_id,
            "PipeWire capture buffer stream differs"
        );
        let exported = client
            .client()
            .export_capture_buffer(stream.stream_id, capture_buffer.buffer_id)
            .with_context(|| {
                format!(
                    "export PipeWire capture buffer {}",
                    capture_buffer.buffer_id
                )
            })?;
        ensure!(
            exported.stream_id == stream.stream_id
                && exported.buffer_id == capture_buffer.buffer_id
                && exported.synchronization == CaptureSynchronization::Explicit,
            "PipeWire capture buffer export identity differs"
        );
        let timelines = exported
            .timelines
            .context("explicit PipeWire export has no sync timelines")?;
        video_buffers.push(VideoBuffer {
            id: exported.buffer_id,
            dma_buf: exported.dma_buf,
            layout: VideoBufferLayout {
                width: exported.layout.width,
                height: exported.layout.height,
                pitch: exported.layout.pitch,
                size: exported.layout.size,
                modifier: exported.layout.modifier,
            },
            timelines: Some(VideoSyncTimelines {
                ready: timelines.ready,
                reuse: timelines.reuse,
            }),
        });
        capture_buffers.push(capture_buffer);
    }

    let grant_id =
        NonZeroU32::new(client.client().grant_id()).context("PipeWire source grant ID is zero")?;
    let connector_id = NonZeroU32::new(client.client().connector_id())
        .context("PipeWire source connector ID is zero")?;
    let node_name = format!(
        "pronk.castkms.grant-{}.generation-{}",
        grant_id, stream.mode_generation
    );
    let config = VideoSourceConfig {
        node_name,
        node_description: description.to_string(),
        session_id: login_session_id.to_string(),
        device_instance: format!("castkms-grant-{grant_id}"),
        connector_id,
        output_index: 0,
        grant_id,
        media_generation: stream.mode_generation,
        refresh_hz: stream.refresh_hz,
    };
    let identity = runtime
        .block_on(actor.start(VideoSourceGeneration {
            config,
            buffers: video_buffers,
            remote: create_pipewire_gate_remote(runtime, remote)?,
        }))
        .context("start PipeWire source actor generation")?;
    ensure!(
        identity.media_generation == stream.mode_generation,
        "PipeWire source media generation differs"
    );

    Ok(PipeWireGateGeneration {
        stream,
        identity,
        capture_buffers,
        available: VecDeque::with_capacity(CAPTURE_POOL_SIZE),
        transport: None,
    })
}

fn initialize_pipewire_gate_generation(
    runtime: &Runtime,
    actor: &mut VideoSourceActor,
    generation: &mut PipeWireGateGeneration,
) -> anyhow::Result<()> {
    ensure!(
        generation.available.is_empty() && generation.transport.is_none(),
        "PipeWire generation was initialized twice"
    );
    let mut transports = HashMap::with_capacity(CAPTURE_POOL_SIZE);
    while generation.available.len() < CAPTURE_POOL_SIZE {
        let event = runtime
            .block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, actor.next_event()).await })
            .context("timed out waiting for initial PipeWire buffer")?
            .context("PipeWire actor stopped before returning its initial buffers")?;
        match event {
            VideoSourceActorEvent::BufferAvailable {
                media_generation,
                buffer_id,
                transport,
            } => {
                ensure!(
                    media_generation == generation.stream.mode_generation,
                    "PipeWire initial buffer belongs to another generation"
                );
                ensure!(
                    transports.insert(buffer_id, transport).is_none(),
                    "PipeWire returned duplicate initial buffer {buffer_id}"
                );
                generation.available.push_back(buffer_id);
            }
            VideoSourceActorEvent::BufferReleased { buffer_id, .. } => {
                bail!("PipeWire released unpublished buffer {buffer_id}")
            }
            VideoSourceActorEvent::GenerationFailed {
                identity, error, ..
            } => bail!(
                "PipeWire generation {} failed during allocation: {error}",
                identity.media_generation
            ),
        }
    }
    let transport = transports
        .values()
        .next()
        .copied()
        .context("PipeWire consumer negotiated no buffer transport")?;
    ensure!(
        transports.values().all(|candidate| *candidate == transport),
        "PipeWire consumer negotiated inconsistent buffer transports"
    );
    generation.transport = Some(transport);
    Ok(())
}

fn produce_pipewire_frames(
    runtime: &Runtime,
    actor: &mut VideoSourceActor,
    client: &mut AsyncCastKmsClient,
    generation: &mut PipeWireGateGeneration,
    frame_count: usize,
    user_data_base: u64,
) -> anyhow::Result<()> {
    initialize_pipewire_gate_generation(runtime, actor, generation)?;
    let transport = generation
        .transport
        .context("PipeWire generation has no negotiated transport")?;
    let mut release_points = HashMap::<NonZeroU32, NonZeroU64>::new();
    for index in 0..frame_count {
        let buffer_id = generation
            .available
            .pop_front()
            .context("PipeWire capture pool became empty")?;
        let user_data = NonZeroU64::new(
            user_data_base
                .checked_add(index as u64)
                .context("PipeWire capture user data overflowed")?,
        )
        .context("PipeWire capture user data is zero")?;
        let queue = client
            .client_mut()
            .queue_capture_buffer(buffer_id, user_data)
            .with_context(|| format!("queue PipeWire capture buffer {buffer_id}"))?;
        let fence = client
            .client()
            .arm_explicit_capture_fence(generation.stream.stream_id, buffer_id)
            .with_context(|| format!("arm PipeWire capture buffer {buffer_id}"))?;
        let event = wait_for_capture_frame(runtime, client, queue)?;
        validate_capture_frame(
            event,
            generation.stream.mode_generation.get(),
            generation.stream.width.get(),
            generation.stream.height.get(),
            queue,
        )?;
        let ready = runtime
            .block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, fence.wait()).await })
            .with_context(|| format!("timed out waiting for PipeWire buffer {buffer_id}"))?
            .with_context(|| format!("wait for PipeWire buffer {buffer_id}"))?;
        let completion = client
            .client_mut()
            .take_capture_completion(ready)
            .with_context(|| format!("take PipeWire capture completion {buffer_id}"))?;
        ensure!(
            completion.queue == queue && completion.frame == event,
            "PipeWire capture completion differs"
        );
        let acquire_point = completion
            .queue
            .ready_point
            .context("explicit PipeWire capture has no ready point")?;
        let frame = VideoFrame {
            buffer_id,
            sequence: event.sequence,
            pts_ns: event.timestamp_ns,
            damage: VideoDamage {
                x: u32::try_from(event.damage_x).context("convert PipeWire damage x")?,
                y: u32::try_from(event.damage_y).context("convert PipeWire damage y")?,
                width: NonZeroU32::new(event.damage_width)
                    .context("PipeWire damage width is zero")?,
                height: NonZeroU32::new(event.damage_height)
                    .context("PipeWire damage height is zero")?,
            },
            discontinuity: event.dropped_frames != 0,
            acquire_point: match transport {
                PipeWireBufferTransport::SyncTimeline => Some(acquire_point),
                PipeWireBufferTransport::Waited => None,
            },
        };
        runtime
            .block_on(async {
                tokio::time::timeout(
                    CAPTURE_TIMEOUT,
                    actor.publish(generation.stream.mode_generation, frame),
                )
                .await
            })
            .with_context(|| format!("timed out publishing PipeWire buffer {buffer_id}"))?
            .with_context(|| format!("publish PipeWire buffer {buffer_id}"))?;

        let released = runtime
            .block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, actor.next_event()).await })
            .with_context(|| format!("timed out waiting for PipeWire release {buffer_id}"))?
            .context("PipeWire actor stopped before releasing a buffer")?;
        match released {
            VideoSourceActorEvent::BufferReleased {
                media_generation,
                buffer_id: released_id,
            } => {
                ensure!(
                    media_generation == generation.stream.mode_generation,
                    "PipeWire release belongs to another generation"
                );
                ensure!(
                    released_id == buffer_id,
                    "PipeWire released a different buffer"
                );
            }
            VideoSourceActorEvent::BufferAvailable {
                buffer_id: unexpected,
                ..
            } => bail!("PipeWire returned unexpected initial buffer {unexpected}"),
            VideoSourceActorEvent::GenerationFailed {
                identity, error, ..
            } => bail!(
                "PipeWire generation {} failed during publication: {error}",
                identity.media_generation
            ),
        }
        let release = client
            .client_mut()
            .release_capture_buffer(generation.stream.stream_id, buffer_id)
            .with_context(|| format!("release PipeWire capture buffer {buffer_id}"))?;
        let reuse_point = release
            .reuse_point
            .context("PipeWire capture release has no reuse point")?;
        if let Some(previous) = release_points.insert(buffer_id, reuse_point) {
            ensure!(
                reuse_point.get() == previous.get() + 1,
                "PipeWire reuse timeline did not advance by one"
            );
        } else {
            ensure!(
                reuse_point == NonZeroU64::new(1).unwrap(),
                "first PipeWire reuse point differs"
            );
        }
        generation.available.push_back(buffer_id);
    }
    Ok(())
}

fn stop_pipewire_gate_generation(
    runtime: &Runtime,
    actor: &VideoSourceActor,
    client: &mut AsyncCastKmsClient,
    generation: &PipeWireGateGeneration,
    require_no_reclaims: bool,
) -> anyhow::Result<()> {
    let report = runtime
        .block_on(actor.stop(generation.stream.mode_generation))
        .context("stop PipeWire actor generation")?;
    ensure!(
        report.identity == generation.identity,
        "stopped PipeWire generation identity differs"
    );
    if require_no_reclaims {
        ensure!(
            report.reclaimed_buffers.is_empty(),
            "PipeWire generation stopped with submitted buffers"
        );
    }
    for buffer_id in report.reclaimed_buffers.iter().copied() {
        client
            .client_mut()
            .release_capture_buffer(generation.stream.stream_id, buffer_id)
            .with_context(|| format!("release reclaimed PipeWire buffer {buffer_id}"))?;
    }
    Ok(())
}

fn unregister_pipewire_gate_generation(
    client: &mut AsyncCastKmsClient,
    generation: &PipeWireGateGeneration,
) -> anyhow::Result<()> {
    for buffer in &generation.capture_buffers {
        let unregistered = client
            .client_mut()
            .unregister_capture_buffer(buffer.buffer_id)
            .with_context(|| format!("unregister PipeWire buffer {}", buffer.buffer_id))?;
        ensure!(
            unregistered.state == CaptureBufferState::Idle,
            "unregistered PipeWire buffer was not idle"
        );
    }
    ensure!(
        client
            .client()
            .capture_buffers(generation.stream.stream_id)
            .is_empty(),
        "PipeWire capture buffers remained after source shutdown"
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct PipeWireModeRestartResult {
    old_stream: CaptureStreamInfo,
    new_stream: CaptureStreamInfo,
    old_identity: VideoNodeIdentity,
    new_identity: VideoNodeIdentity,
    old_frames: usize,
    new_frames: usize,
}

fn exercise_pipewire_mode_restart_gate(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    old_stream: CaptureStreamInfo,
    crtc_id: NonZeroU32,
    expected_grant_id: u32,
    login_session_id: &str,
    remote: PipeWireGateRemote,
) -> anyhow::Result<PipeWireModeRestartResult> {
    let mut mode_change_signal = {
        let _runtime_guard = runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .context("install PipeWire mode-change SIGUSR1 handler")?
    };
    let mut actor = {
        let _runtime_guard = runtime.enter();
        VideoSourceActor::spawn().context("spawn mode-restart PipeWire source actor")?
    };
    let mut old_generation = start_pipewire_gate_generation(
        runtime,
        &actor,
        client,
        old_stream,
        login_session_id,
        "Pronk CastKMS old-mode VM gate",
        remote,
    )?;
    println!(
        "pipewire_mode_old_source_ready={}:{}:{}",
        old_generation.identity.node_name,
        old_generation.identity.object_id,
        old_generation.identity.object_serial
    );
    std::io::stdout()
        .flush()
        .context("flush old PipeWire source readiness marker")?;
    produce_pipewire_frames(
        runtime,
        &mut actor,
        client,
        &mut old_generation,
        PIPEWIRE_MODE_GENERATION_FRAME_COUNT,
        CAPTURE_USER_DATA + 0x200,
    )?;

    println!(
        "pipewire_mode_change_trigger_ready={}x{}@generation-{}",
        old_stream.width, old_stream.height, old_stream.mode_generation
    );
    std::io::stdout()
        .flush()
        .context("flush PipeWire mode-change readiness marker")?;
    runtime
        .block_on(async {
            tokio::time::timeout(MODE_CHANGE_TIMEOUT, mode_change_signal.recv()).await
        })
        .context("timed out waiting for PipeWire mode-change acknowledgement")?
        .context("PipeWire mode-change signal stream closed")?;

    stop_pipewire_gate_generation(runtime, &actor, client, &old_generation, true)?;
    let stale_buffer = old_generation.capture_buffers[0];
    let stale_user_data =
        NonZeroU64::new(CAPTURE_USER_DATA + 0x300).expect("stale user data is nonzero");
    ensure!(
        matches!(
            client
                .client_mut()
                .queue_capture_buffer(stale_buffer.buffer_id, stale_user_data),
            Err(CaptureError::QueueBuffer(Errno::ESTALE))
        ),
        "old PipeWire generation queue did not return ESTALE"
    );
    ensure!(
        client.client().active_capture_requires_restart(),
        "PipeWire mode-change ESTALE did not mark the stream stale"
    );
    let stopped = client
        .client_mut()
        .stop_capture()
        .context("stop stale PipeWire capture stream")?;
    ensure!(stopped.stream == old_stream, "stopped old stream differs");
    ensure!(
        stopped.waiting_buffer_count == 0,
        "old PipeWire pool retained waiting buffers"
    );
    let retired = client
        .client_mut()
        .finish_retired_capture(old_stream.stream_id)
        .context("destroy old PipeWire capture pool")?;
    ensure!(retired.stream == old_stream, "retired old stream differs");
    ensure!(
        retired.buffers.len() == CAPTURE_POOL_SIZE
            && retired.buffers.iter().all(|buffer| {
                buffer.state == CaptureBufferState::Idle
                    && buffer.synchronization == CaptureSynchronization::Explicit
            }),
        "retired old PipeWire pool differs"
    );

    let grant = client
        .client()
        .query_grant()
        .context("query grant before replacement PipeWire capture")?;
    ensure!(
        grant.grant_id == expected_grant_id && grant.state == GrantState::Active,
        "grant changed before replacement PipeWire capture"
    );
    let capabilities = client
        .client()
        .query_capture_capabilities(crtc_id)
        .context("query replacement PipeWire capture capabilities")?;
    let new_stream = client
        .client_mut()
        .start_capture(&capabilities, old_stream.cursor_mode)
        .context("start replacement PipeWire capture stream")?;
    ensure!(
        new_stream.mode_generation > old_stream.mode_generation,
        "replacement PipeWire generation is not newer"
    );
    ensure!(
        (new_stream.width, new_stream.height) != (old_stream.width, old_stream.height),
        "PipeWire VM orchestrator did not change the mode dimensions"
    );

    let mut new_generation = start_pipewire_gate_generation(
        runtime,
        &actor,
        client,
        new_stream,
        login_session_id,
        "Pronk CastKMS replacement-mode VM gate",
        remote,
    )?;
    ensure!(
        new_generation.identity.object_serial != old_generation.identity.object_serial,
        "replacement PipeWire source reused the old object serial"
    );
    ensure!(
        new_generation.identity.node_name != old_generation.identity.node_name,
        "replacement PipeWire source reused the old node name"
    );
    println!(
        "pipewire_mode_new_source_ready={}:{}:{}",
        new_generation.identity.node_name,
        new_generation.identity.object_id,
        new_generation.identity.object_serial
    );
    std::io::stdout()
        .flush()
        .context("flush replacement PipeWire source readiness marker")?;
    produce_pipewire_frames(
        runtime,
        &mut actor,
        client,
        &mut new_generation,
        PIPEWIRE_MODE_GENERATION_FRAME_COUNT,
        CAPTURE_USER_DATA + 0x400,
    )?;
    stop_pipewire_gate_generation(runtime, &actor, client, &new_generation, true)?;
    unregister_pipewire_gate_generation(client, &new_generation)?;
    let stopped = client
        .client_mut()
        .stop_capture()
        .context("stop replacement PipeWire capture stream")?;
    ensure!(
        stopped.stream == new_stream && stopped.waiting_buffer_count == 0,
        "replacement PipeWire stream did not stop cleanly"
    );
    ensure!(
        runtime
            .block_on(actor.shutdown())
            .context("shut down mode-restart PipeWire actor")?
            .is_none(),
        "mode-restart PipeWire actor retained a generation"
    );

    Ok(PipeWireModeRestartResult {
        old_stream,
        new_stream,
        old_identity: old_generation.identity,
        new_identity: new_generation.identity,
        old_frames: PIPEWIRE_MODE_GENERATION_FRAME_COUNT,
        new_frames: PIPEWIRE_MODE_GENERATION_FRAME_COUNT,
    })
}

#[derive(Debug, Clone, Copy)]
struct MasterReopenResult {
    old_stream: CaptureStreamInfo,
    new_stream: CaptureStreamInfo,
    replacement_hash: u64,
}

fn exercise_master_reopen_gate(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    old_stream: CaptureStreamInfo,
    crtc_id: NonZeroU32,
    edid: &ValidatedEdid,
    connector_sysfs: &Path,
    old_buffer: pronk_core::castkms::CaptureBufferInfo,
) -> anyhow::Result<MasterReopenResult> {
    let expected_grant_id = client.client().grant_id();
    ensure!(
        old_buffer.stream_id == old_stream.stream_id
            && old_buffer.synchronization == CaptureSynchronization::Explicit
            && old_buffer.state == CaptureBufferState::Idle,
        "pre-master-reopen buffer is not the reusable explicit buffer"
    );
    println!(
        "master_reopen_trigger_ready={}x{}@generation-{}",
        old_stream.width, old_stream.height, old_stream.mode_generation
    );
    std::io::stdout()
        .flush()
        .context("flush master-reopen readiness marker")?;

    let cancellation_user_data =
        NonZeroU64::new(CAPTURE_USER_DATA + 5).expect("cancellation user data is nonzero");
    let cancellation_queue = client
        .client_mut()
        .queue_capture_buffer(old_buffer.buffer_id, cancellation_user_data)
        .context("queue explicit buffer for master-cleanup cancellation")?;
    ensure!(
        cancellation_queue.ready_point == NonZeroU64::new(3)
            && cancellation_queue.reuse_point == NonZeroU64::new(2),
        "master-cleanup queue did not continue the explicit timelines"
    );
    let cancellation_fence = client
        .client()
        .arm_explicit_capture_fence(old_stream.stream_id, old_buffer.buffer_id)
        .context("arm master-cleanup cancellation fence")?;
    client
        .client()
        .detach_monitor()
        .context("detach monitor to make Mutter close CastKMS")?;
    wait_for_status(connector_sysfs, "disconnected")?;
    wait_for_edid(connector_sysfs, &[])?;
    let reconciliation = client
        .client_mut()
        .reconcile_grant_state(GrantStateEvidence::CaptureInvalidated(Errno::ENOTCONN))
        .context("retire capture invalidated by monitor detach")?;
    ensure!(
        reconciliation.grant.grant_id == expected_grant_id,
        "master-reopen grant ID differs after detach"
    );
    let GrantCaptureReconciliation::Retired(stopped) = reconciliation.capture else {
        bail!(
            "monitor detach did not retire the old stream: {:?}",
            reconciliation.capture
        )
    };
    ensure!(
        stopped.stream == old_stream,
        "detached stream metadata differs"
    );
    ensure!(
        stopped.kernel_stream_was_gone,
        "detach left the old kernel capture stream alive"
    );
    ensure!(
        stopped.waiting_buffer_count == 1,
        "queued pre-master-reopen buffer was not retained for drain"
    );
    let cancellation_event = wait_for_retired_capture_frame(runtime, client, cancellation_queue)?;
    ensure!(
        cancellation_event.status == -(Errno::ENOTCONN as i32),
        "detached capture completed with {} instead of -ENOTCONN",
        cancellation_event.status
    );
    ensure!(
        cancellation_event.flags == 0,
        "detached capture completion has unexpected flags"
    );
    let cancellation_ready = runtime
        .block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, cancellation_fence.wait()).await })
        .context("timed out waiting for canceled explicit ready point")?
        .context("wait for canceled explicit ready point")?;
    let cancellation_completion = client
        .client_mut()
        .take_capture_completion(cancellation_ready)
        .context("take canceled retired-stream completion")?;
    ensure!(
        cancellation_completion.queue == cancellation_queue
            && cancellation_completion.frame == cancellation_event,
        "canceled retired-stream completion identity differs"
    );
    let cancellation_release = client
        .client_mut()
        .release_capture_buffer(old_stream.stream_id, old_buffer.buffer_id)
        .context("release canceled retired-stream buffer")?;
    ensure!(
        cancellation_release.reuse_point == NonZeroU64::new(3),
        "canceled explicit buffer did not advance reuse point 3"
    );
    let retired = client
        .client_mut()
        .finish_retired_capture(old_stream.stream_id)
        .context("destroy pre-master-reopen capture pool")?;
    ensure!(
        retired.stream == old_stream,
        "cleaned detached stream differs"
    );
    ensure!(retired.buffers.len() == 1, "detached pool size differs");
    ensure!(
        retired.buffers[0].buffer_id == old_buffer.buffer_id
            && retired.buffers[0].state == CaptureBufferState::Idle,
        "detached old-buffer metadata differs"
    );

    wait_for_exact_grant_state(
        runtime,
        client,
        expected_grant_id,
        GrantState::SuspendedNoMaster,
    )?;
    println!("master_reopen_attach_ready=suspended-no-master");
    std::io::stdout()
        .flush()
        .context("flush master-reopen attach marker")?;

    client
        .client()
        .attach_monitor(edid, "CastKMS Live Test")
        .context("reattach monitor after loss of DRM master")?;
    wait_for_status(connector_sysfs, "connected")?;
    wait_for_edid(connector_sysfs, edid.as_bytes())?;
    let saw_foreign_content = wait_for_master_reactivation(runtime, client, expected_grant_id)?;
    ensure!(
        saw_foreign_content,
        "master reopen did not expose a foreign-content safety interval"
    );
    let (new_stream, replacement_hash) = capture_one_replacement_frame(
        runtime,
        client,
        old_stream.cursor_mode,
        crtc_id,
        expected_grant_id,
    )?;
    ensure!(
        (new_stream.width, new_stream.height) == (old_stream.width, old_stream.height),
        "master reopen changed the active mode dimensions"
    );

    Ok(MasterReopenResult {
        old_stream,
        new_stream,
        replacement_hash,
    })
}

fn wait_for_exact_grant_state(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    expected_grant_id: u32,
    expected_state: GrantState,
) -> anyhow::Result<()> {
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + GRANT_SUSPENSION_TIMEOUT;
        loop {
            let reconciliation = client
                .client_mut()
                .reconcile_grant_state(GrantStateEvidence::Query)
                .context("query grant while waiting for exact state")?;
            ensure!(
                reconciliation.grant.grant_id == expected_grant_id,
                "exact-state query grant ID differs"
            );
            ensure!(
                reconciliation.capture == GrantCaptureReconciliation::NoCapture,
                "capture resources remained while waiting for exact state"
            );
            if reconciliation.grant.state == expected_state {
                return Ok(());
            }
            ensure!(
                !matches!(
                    reconciliation.grant.state,
                    GrantState::SuspendedOtherMaster | GrantState::Revoked
                ),
                "grant became {:?} while waiting for {:?}",
                reconciliation.grant.state,
                expected_state
            );

            let now = tokio::time::Instant::now();
            ensure!(now < deadline, "timed out waiting for {expected_state:?}");
            let wait = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(100));
            let events = match tokio::time::timeout(wait, client.read_events()).await {
                Ok(events) => events.context("read exact-state grant event")?,
                Err(_) => continue,
            };
            for event in events {
                match event {
                    CastKmsEvent::GrantState(event) => {
                        let reconciliation = client
                            .client_mut()
                            .reconcile_grant_state(GrantStateEvidence::Event(event))
                            .context("reconcile exact-state grant event")?;
                        ensure!(
                            reconciliation.capture == GrantCaptureReconciliation::NoCapture,
                            "exact-state event found capture resources"
                        );
                        if reconciliation.grant.state == expected_state {
                            return Ok(());
                        }
                    }
                    CastKmsEvent::GrantRevoked(event) => {
                        bail!(
                            "grant was revoked while waiting for {expected_state:?}: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CaptureFrame(event) => {
                        bail!(
                            "detached idle stream completed unexpectedly: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                }
            }
        }
    })
}

fn wait_for_retired_capture_frame(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    queue: CaptureQueue,
) -> anyhow::Result<CaptureFrameEvent> {
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + CAPTURE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let events = tokio::time::timeout(remaining, client.read_events())
                .await
                .context("timed out waiting for retired-stream completion")?
                .context("read retired-stream completion")?;
            for event in events {
                match event {
                    CastKmsEvent::CaptureFrame(event) => {
                        ensure!(
                            event.stream_id == queue.stream_id.get()
                                && event.buffer_id == queue.buffer_id.get()
                                && event.user_data == queue.user_data.get(),
                            "retired-stream completion identity differs"
                        );
                        return Ok(event);
                    }
                    CastKmsEvent::GrantState(event) => {
                        let reconciliation = client
                            .client_mut()
                            .reconcile_grant_state(GrantStateEvidence::Event(event))
                            .context("reconcile state while draining retired stream")?;
                        ensure!(
                            matches!(
                                reconciliation.capture,
                                GrantCaptureReconciliation::Retiring(stream)
                                    if stream.stream_id == queue.stream_id
                            ),
                            "grant state lost the retiring stream"
                        );
                    }
                    CastKmsEvent::GrantRevoked(event) => {
                        bail!(
                            "grant was revoked while draining retired stream: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                }
            }
        }
    })
}

fn wait_for_master_reactivation(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    expected_grant_id: u32,
) -> anyhow::Result<bool> {
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + GRANT_SUSPENSION_TIMEOUT;
        let mut saw_foreign_content = false;
        loop {
            let now = tokio::time::Instant::now();
            ensure!(now < deadline, "timed out waiting for master reactivation");
            let wait = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(100));
            if let Ok(events) = tokio::time::timeout(wait, client.read_events()).await {
                for event in events.context("read master-reactivation event")? {
                    match event {
                        CastKmsEvent::GrantState(event) => {
                            saw_foreign_content |=
                                event.state == GrantState::SuspendedForeignContent;
                            let reconciliation = client
                                .client_mut()
                                .reconcile_grant_state(GrantStateEvidence::Event(event))
                                .context("reconcile master-reactivation event")?;
                            ensure!(
                                reconciliation.capture == GrantCaptureReconciliation::NoCapture,
                                "master-reactivation event found capture resources"
                            );
                        }
                        CastKmsEvent::GrantRevoked(event) => {
                            bail!(
                                "grant was revoked during master reactivation: status {}",
                                event.status
                            )
                        }
                        CastKmsEvent::CaptureFrame(event) => {
                            bail!(
                                "retired stream completed during master reactivation: status {}",
                                event.status
                            )
                        }
                        CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                    }
                }
            }

            let reconciliation = client
                .client_mut()
                .reconcile_grant_state(GrantStateEvidence::Query)
                .context("query grant during master reactivation")?;
            ensure!(
                reconciliation.grant.grant_id == expected_grant_id,
                "master-reactivation grant ID differs"
            );
            ensure!(
                reconciliation.capture == GrantCaptureReconciliation::NoCapture,
                "capture resources remained during master reactivation"
            );
            if reconciliation.grant.state == GrantState::Active {
                return Ok(saw_foreign_content);
            }
            ensure!(
                !matches!(
                    reconciliation.grant.state,
                    GrantState::SuspendedOtherMaster | GrantState::Revoked
                ),
                "grant became {:?} during master reactivation",
                reconciliation.grant.state
            );
        }
    })
}

#[derive(Debug, Clone, Copy)]
struct GrantSuspensionResult {
    suspended_state: GrantState,
    old_stream: CaptureStreamInfo,
    new_stream: CaptureStreamInfo,
    replacement_hash: u64,
}

fn exercise_grant_suspension_gate(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    old_stream: CaptureStreamInfo,
    crtc_id: NonZeroU32,
    expected_grant_id: u32,
) -> anyhow::Result<GrantSuspensionResult> {
    let old_buffer = client
        .client_mut()
        .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Explicit)
        .context("allocate pre-suspension capture buffer")?;
    let old_layout = old_buffer
        .layout
        .context("pre-suspension buffer has no layout")?;
    ensure!(
        old_layout.width == old_stream.width && old_layout.height == old_stream.height,
        "pre-suspension buffer does not match the old stream"
    );

    println!(
        "grant_suspension_trigger_ready={}x{}@generation-{}",
        old_stream.width, old_stream.height, old_stream.mode_generation
    );
    std::io::stdout()
        .flush()
        .context("flush grant-suspension readiness marker")?;

    let (suspended_state, stopped) =
        wait_for_nonactive_grant_state(runtime, client, expected_grant_id)?;
    ensure!(stopped.stream == old_stream, "suspended old stream differs");
    ensure!(
        stopped.waiting_buffer_count == 0,
        "idle pre-suspension buffer required an asynchronous drain"
    );
    let retired = client
        .client_mut()
        .finish_retired_capture(old_stream.stream_id)
        .context("destroy retired pre-suspension capture pool")?;
    ensure!(
        retired.stream == old_stream,
        "cleaned suspended stream differs"
    );
    ensure!(retired.buffers.len() == 1, "suspended pool size differs");
    ensure!(
        retired.buffers[0].buffer_id == old_buffer.buffer_id
            && retired.buffers[0].state == CaptureBufferState::Idle
            && retired.buffers[0].synchronization == CaptureSynchronization::Explicit,
        "retired pre-suspension buffer metadata differs"
    );

    println!(
        "grant_resume_trigger_ready={}",
        grant_state_label(suspended_state)
    );
    std::io::stdout()
        .flush()
        .context("flush grant-resume readiness marker")?;
    wait_for_reactivated_grant(runtime, client, expected_grant_id)?;

    let (new_stream, replacement_hash) = capture_one_replacement_frame(
        runtime,
        client,
        old_stream.cursor_mode,
        crtc_id,
        expected_grant_id,
    )?;
    ensure!(
        new_stream.mode_generation != old_stream.mode_generation,
        "output disable/re-enable retained the old mode generation"
    );
    ensure!(
        (new_stream.width, new_stream.height) == (old_stream.width, old_stream.height),
        "output-disable gate did not restore the original mode dimensions"
    );

    Ok(GrantSuspensionResult {
        suspended_state,
        old_stream,
        new_stream,
        replacement_hash,
    })
}

fn wait_for_nonactive_grant_state(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    expected_grant_id: u32,
) -> anyhow::Result<(GrantState, CaptureStopOutcome)> {
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + GRANT_SUSPENSION_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let events = tokio::time::timeout(remaining, client.read_events())
                .await
                .context("timed out waiting for non-active grant state")?
                .context("read grant-suspension event")?;
            for event in events {
                match event {
                    CastKmsEvent::GrantState(event) => {
                        let observed_state = event.state;
                        let reconciliation = client
                            .client_mut()
                            .reconcile_grant_state(GrantStateEvidence::Event(event))
                            .context("reconcile grant-suspension state event")?;
                        ensure!(
                            reconciliation.grant.grant_id == expected_grant_id,
                            "suspension reconciliation grant ID differs"
                        );
                        if observed_state == GrantState::Active {
                            continue;
                        }
                        ensure!(
                            !matches!(
                                reconciliation.grant.state,
                                GrantState::Active
                                    | GrantState::SuspendedOtherMaster
                                    | GrantState::Revoked
                            ),
                            "grant suspension resolved to {:?}",
                            reconciliation.grant.state
                        );
                        let GrantCaptureReconciliation::Retired(stopped) =
                            reconciliation.capture
                        else {
                            bail!(
                                "non-active transition did not retire the old stream: {:?}",
                                reconciliation.capture
                            )
                        };
                        return Ok((reconciliation.grant.state, stopped));
                    }
                    CastKmsEvent::GrantRevoked(event) => {
                        bail!(
                            "grant was revoked during output suspension: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CaptureFrame(event) => {
                        bail!(
                            "idle stream completed unexpectedly during output suspension: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                }
            }
        }
    })
}

fn wait_for_reactivated_grant(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    expected_grant_id: u32,
) -> anyhow::Result<()> {
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + GRANT_SUSPENSION_TIMEOUT;
        loop {
            let reconciliation = client
                .client_mut()
                .reconcile_grant_state(GrantStateEvidence::Query)
                .context("authoritatively query suspended grant")?;
            ensure!(
                reconciliation.grant.grant_id == expected_grant_id,
                "reactivation query grant ID differs"
            );
            ensure!(
                reconciliation.capture == GrantCaptureReconciliation::NoCapture,
                "capture resources remained while awaiting reactivation"
            );
            if reconciliation.grant.state == GrantState::Active {
                return Ok(());
            }
            ensure!(
                !matches!(
                    reconciliation.grant.state,
                    GrantState::SuspendedOtherMaster | GrantState::Revoked
                ),
                "grant became {:?} while awaiting reactivation",
                reconciliation.grant.state
            );

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let events = tokio::time::timeout(remaining, client.read_events())
                .await
                .context("timed out waiting for grant reactivation")?
                .context("read grant-reactivation event")?;
            for event in events {
                match event {
                    CastKmsEvent::GrantState(event) => {
                        let reconciliation = client
                            .client_mut()
                            .reconcile_grant_state(GrantStateEvidence::Event(event))
                            .context("reconcile grant-reactivation state event")?;
                        ensure!(
                            reconciliation.capture == GrantCaptureReconciliation::NoCapture,
                            "reactivation event found capture resources"
                        );
                        if reconciliation.grant.state == GrantState::Active {
                            return Ok(());
                        }
                    }
                    CastKmsEvent::GrantRevoked(event) => {
                        bail!(
                            "grant was revoked while awaiting reactivation: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CaptureFrame(event) => {
                        bail!(
                            "retired stream completed unexpectedly while awaiting reactivation: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                }
            }
        }
    })
}

fn grant_state_label(state: GrantState) -> &'static str {
    match state {
        GrantState::Pending => "pending",
        GrantState::Active => "active",
        GrantState::SuspendedNoMaster => "suspended-no-master",
        GrantState::SuspendedOtherMaster => "suspended-other-master",
        GrantState::SuspendedForeignContent => "suspended-foreign-content",
        GrantState::Revoked => "revoked",
    }
}

#[derive(Debug, Clone, Copy)]
struct ModeRestartResult {
    old_stream: CaptureStreamInfo,
    new_stream: CaptureStreamInfo,
    replacement_hash: u64,
}

/// Exercise a compositor-driven modeset without racing a queued vblank.
///
/// The caller installs the SIGUSR1 handler before publishing the readiness
/// marker. The VM orchestrator changes the CastKMS monitor's mode, waits for
/// `gdctl set` to complete, and then sends SIGUSR1 to this process. Queueing an
/// old-generation idle buffer must return ESTALE and drive the same rebuild as
/// an asynchronous MODE_CHANGED completion would.
fn exercise_mode_restart_gate(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    old_stream: CaptureStreamInfo,
    crtc_id: NonZeroU32,
    expected_grant_id: u32,
) -> anyhow::Result<ModeRestartResult> {
    let old_buffer = client
        .client_mut()
        .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Explicit)
        .context("allocate old-mode restart buffer")?;
    let old_layout = old_buffer
        .layout
        .context("old-mode restart buffer has no layout")?;
    ensure!(
        old_layout.width == old_stream.width,
        "old buffer width differs"
    );
    ensure!(
        old_layout.height == old_stream.height,
        "old buffer height differs"
    );

    let mut mode_change_signal = {
        let _runtime_guard = runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .context("install mode-change SIGUSR1 handler")?
    };
    println!(
        "mode_change_trigger_ready={}x{}@generation-{}",
        old_stream.width, old_stream.height, old_stream.mode_generation
    );
    std::io::stdout()
        .flush()
        .context("flush mode-change readiness marker")?;
    runtime
        .block_on(async {
            tokio::time::timeout(MODE_CHANGE_TIMEOUT, mode_change_signal.recv()).await
        })
        .context("timed out waiting for compositor mode-change acknowledgement")?
        .context("mode-change signal stream closed")?;

    let stale_user_data =
        NonZeroU64::new(CAPTURE_USER_DATA + 3).expect("stale queue user data is nonzero");
    ensure!(
        matches!(
            client
                .client_mut()
                .queue_capture_buffer(old_buffer.buffer_id, stale_user_data),
            Err(CaptureError::QueueBuffer(Errno::ESTALE))
        ),
        "old-generation queue did not return ESTALE"
    );
    ensure!(
        client.client().active_capture_requires_restart(),
        "synchronous ESTALE did not mark the stream stale"
    );
    ensure!(
        client.client().active_capture_stream() == Some(old_stream),
        "stale stream identity changed before retirement"
    );

    let stopped = client
        .client_mut()
        .stop_capture()
        .context("stop stale old-mode stream")?;
    ensure!(stopped.stream == old_stream, "retired old stream differs");
    ensure!(
        !stopped.kernel_stream_was_gone,
        "modeset unexpectedly destroyed the old kernel stream"
    );
    ensure!(
        stopped.waiting_buffer_count == 0,
        "idle old-mode buffer required an asynchronous drain"
    );
    let retired = client
        .client_mut()
        .finish_retired_capture(old_stream.stream_id)
        .context("destroy retired old-mode pool")?;
    ensure!(retired.stream == old_stream, "cleaned old stream differs");
    ensure!(retired.buffers.len() == 1, "retired pool size differs");
    ensure!(
        retired.buffers[0].buffer_id == old_buffer.buffer_id
            && retired.buffers[0].state == CaptureBufferState::Idle
            && retired.buffers[0].synchronization == CaptureSynchronization::Explicit,
        "retired old-mode buffer metadata differs"
    );
    ensure!(
        client
            .client()
            .capture_buffers(old_stream.stream_id)
            .is_empty(),
        "old-mode resources remained tracked after cleanup"
    );

    let (new_stream, replacement_hash) = capture_one_replacement_frame(
        runtime,
        client,
        old_stream.cursor_mode,
        crtc_id,
        expected_grant_id,
    )?;
    ensure!(
        new_stream.mode_generation != old_stream.mode_generation,
        "replacement stream retained the old mode generation"
    );
    ensure!(
        (new_stream.width, new_stream.height) != (old_stream.width, old_stream.height),
        "VM orchestrator did not change the physical mode dimensions"
    );
    ensure!(
        !client.client().active_capture_requires_restart(),
        "replacement stream began stale"
    );

    Ok(ModeRestartResult {
        old_stream,
        new_stream,
        replacement_hash,
    })
}

fn capture_one_replacement_frame(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    cursor_mode: CursorCaptureMode,
    crtc_id: NonZeroU32,
    expected_grant_id: u32,
) -> anyhow::Result<(CaptureStreamInfo, u64)> {
    let grant = client
        .client()
        .query_grant()
        .context("query grant before replacement capture")?;
    ensure!(grant.grant_id == expected_grant_id, "grant ID changed");
    ensure!(grant.state == GrantState::Active, "grant is not active");

    let capabilities = client
        .client()
        .query_capture_capabilities(crtc_id)
        .context("requery replacement capture capabilities")?;
    let new_stream = client
        .client_mut()
        .start_capture(&capabilities, cursor_mode)
        .context("start replacement capture stream")?;

    let replacement_buffer = client
        .client_mut()
        .allocate_linear_xrgb8888_buffer(CaptureSynchronization::Explicit)
        .context("allocate replacement-mode buffer")?;
    let replacement_layout = replacement_buffer
        .layout
        .context("replacement-mode buffer has no layout")?;
    ensure!(
        replacement_layout.width == new_stream.width
            && replacement_layout.height == new_stream.height,
        "replacement buffer does not match the new mode"
    );
    let replacement_sentinel_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(new_stream.stream_id, replacement_buffer.buffer_id)
            .context("borrow replacement-mode DMA-BUF")?;
        write_dma_buf_pattern(dma_buf, replacement_layout.size, 0x33)
            .context("prefill replacement-mode DMA-BUF")?
    };
    let replacement_user_data =
        NonZeroU64::new(CAPTURE_USER_DATA + 4).expect("replacement user data is nonzero");
    let replacement_queue = client
        .client_mut()
        .queue_capture_buffer(replacement_buffer.buffer_id, replacement_user_data)
        .context("queue replacement-mode buffer")?;
    ensure!(
        replacement_queue.ready_point == NonZeroU64::new(1)
            && replacement_queue.reuse_point.is_none(),
        "replacement explicit queue points differ"
    );
    let replacement_fence = client
        .client()
        .arm_explicit_capture_fence(new_stream.stream_id, replacement_buffer.buffer_id)
        .context("arm replacement ready point")?;
    let replacement_event = wait_for_capture_frame(runtime, client, replacement_queue)?;
    validate_capture_frame(
        replacement_event,
        new_stream.mode_generation.get(),
        new_stream.width.get(),
        new_stream.height.get(),
        replacement_queue,
    )?;
    let replacement_ready = runtime
        .block_on(async { tokio::time::timeout(CAPTURE_TIMEOUT, replacement_fence.wait()).await })
        .context("timed out waiting for replacement ready point")?
        .context("wait for replacement ready point")?;
    let replacement_completion = client
        .client_mut()
        .take_capture_completion(replacement_ready)
        .context("take replacement capture completion")?;
    ensure!(
        replacement_completion.queue == replacement_queue
            && replacement_completion.frame == replacement_event,
        "replacement completion identity differs"
    );
    let replacement_hash = {
        let dma_buf = client
            .client()
            .capture_dma_buf(new_stream.stream_id, replacement_buffer.buffer_id)
            .context("borrow completed replacement DMA-BUF")?;
        hash_dma_buf(dma_buf, replacement_layout.size)
            .context("hash completed replacement DMA-BUF")?
    };
    ensure!(
        replacement_hash != replacement_sentinel_hash,
        "replacement capture left sentinel pixels unchanged"
    );
    let replacement_release = client
        .client_mut()
        .release_capture_buffer(new_stream.stream_id, replacement_buffer.buffer_id)
        .context("release replacement capture buffer")?;
    ensure!(
        replacement_release.reuse_point == NonZeroU64::new(1),
        "replacement reuse point differs"
    );
    client
        .client_mut()
        .unregister_capture_buffer(replacement_buffer.buffer_id)
        .context("destroy replacement buffer and syncobjs")?;
    let stopped = client
        .client_mut()
        .stop_capture()
        .context("stop replacement capture stream")?;
    ensure!(stopped.stream == new_stream, "stopped replacement differs");
    ensure!(
        stopped.waiting_buffer_count == 0,
        "replacement stream retained buffers"
    );
    Ok((new_stream, replacement_hash))
}

struct MappedDmaBuf {
    address: NonNull<u8>,
    length: usize,
}

impl MappedDmaBuf {
    fn new(fd: BorrowedFd<'_>, size: NonZeroU64) -> anyhow::Result<Self> {
        let length = usize::try_from(size.get()).context("DMA-BUF is too large to map")?;
        // SAFETY: the DMA-BUF remains borrowed for this call, length is the
        // validated allocation size, and a successful MAP_SHARED mapping is
        // retained by this RAII object until `munmap` in Drop.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("map capture DMA-BUF");
        }
        let Some(address) = NonNull::new(address.cast::<u8>()) else {
            // SAFETY: mmap reported success, so even an unusable null result
            // still names the mapping that this constructor must release.
            let _ = unsafe { libc::munmap(address, length) };
            bail!("mmap returned a null address");
        };
        Ok(Self { address, length })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `address` names a live writable mapping of exactly `length`
        // bytes uniquely borrowed through `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.address.as_ptr(), self.length) }
    }
}

impl Drop for MappedDmaBuf {
    fn drop(&mut self) {
        // SAFETY: this object owns the live mapping returned by mmap and calls
        // munmap exactly once with the original address and length.
        let _ = unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

fn write_dma_buf_pattern(fd: BorrowedFd<'_>, size: NonZeroU64, pattern: u8) -> anyhow::Result<u64> {
    with_dma_buf_access(fd, size, DMA_BUF_SYNC_WRITE, |bytes| {
        bytes.fill(pattern);
        hash_bytes(bytes)
    })
}

fn hash_dma_buf(fd: BorrowedFd<'_>, size: NonZeroU64) -> anyhow::Result<u64> {
    with_dma_buf_access(fd, size, DMA_BUF_SYNC_READ, |bytes| hash_bytes(bytes))
}

fn with_dma_buf_access<R>(
    fd: BorrowedFd<'_>,
    size: NonZeroU64,
    access: u64,
    operation: impl FnOnce(&mut [u8]) -> R,
) -> anyhow::Result<R> {
    sync_dma_buf(fd, DMA_BUF_SYNC_START | access).context("begin DMA-BUF CPU access")?;
    let result = MappedDmaBuf::new(fd, size).map(|mut mapping| operation(mapping.bytes_mut()));
    let end_result = sync_dma_buf(fd, DMA_BUF_SYNC_END | access).context("end DMA-BUF CPU access");
    match (result, end_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn sync_dma_buf(fd: BorrowedFd<'_>, flags: u64) -> Result<(), Errno> {
    let args = DmaBufSync { flags };
    // SAFETY: `args` has the checked-in DMA-BUF UAPI layout and remains valid
    // for the duration of the synchronous ioctl.
    unsafe { dma_buf_ioctl_sync(fd.as_raw_fd(), &args) }?;
    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

fn diagnostic_edid(product_code: u16) -> anyhow::Result<ValidatedEdid> {
    let mut edid = vec![0_u8; 128];
    edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    edid[8] = 0x0d;
    edid[9] = 0x6d;
    edid[10..12].copy_from_slice(&product_code.to_le_bytes());
    edid[16] = 1;
    edid[17] = 34;
    edid[18] = 1;
    edid[19] = 3;
    edid[20] = 0x80;
    edid[23] = 120;
    edid[24] = 0x0a;
    edid[25] = 0xee;
    edid[26] = 0x91;
    edid[27] = 0xa3;
    edid[28] = 0x54;
    edid[29] = 0x4c;
    edid[30] = 0x99;
    edid[31] = 0x26;
    edid[32] = 0x0f;
    edid[33] = 0x50;
    edid[34] = 0x54;
    edid[35] = 0x21;
    edid[36] = 0x08;
    edid[38] = (1920 / 8) as u8 - 31;
    edid[39] = 0xc0;
    for index in (40..54).step_by(2) {
        edid[index] = 0x01;
        edid[index + 1] = 0x01;
    }
    write_dtd(&mut edid[54..72], 14_850, 1920, 280, 1080, 45, 88, 44, 4, 5);
    // Unused descriptor. This diagnostic fixture intentionally has no 0xFC
    // base-block product-name descriptor; production identity will use
    // DisplayID Product Identification instead.
    edid[75] = 0x10;
    write_dtd(
        &mut edid[90..108],
        59_400,
        3840,
        560,
        2160,
        90,
        176,
        88,
        8,
        10,
    );
    edid[111] = 0x10;
    let checksum = edid[..127].iter().copied().fold(0_u8, u8::wrapping_add);
    edid[127] = 0_u8.wrapping_sub(checksum);

    ValidatedEdid::new(edid).context("validate diagnostic EDID")
}

#[allow(clippy::too_many_arguments)]
fn write_dtd(
    dtd: &mut [u8],
    clock_10khz: u32,
    hactive: u32,
    hblank: u32,
    vactive: u32,
    vblank: u32,
    hfront: u32,
    hsync: u32,
    vfront: u32,
    vsync: u32,
) {
    dtd[0] = clock_10khz as u8;
    dtd[1] = (clock_10khz >> 8) as u8;
    dtd[2] = hactive as u8;
    dtd[3] = hblank as u8;
    dtd[4] = ((hactive >> 4) as u8 & 0xf0) | ((hblank >> 8) as u8 & 0x0f);
    dtd[5] = vactive as u8;
    dtd[6] = vblank as u8;
    dtd[7] = ((vactive >> 4) as u8 & 0xf0) | ((vblank >> 8) as u8 & 0x0f);
    dtd[8] = hfront as u8;
    dtd[9] = hsync as u8;
    dtd[10] = ((vfront & 0x0f) << 4) as u8 | (vsync & 0x0f) as u8;
    dtd[11] = (((hfront >> 8) & 0x03) << 6) as u8
        | (((hsync >> 8) & 0x03) << 4) as u8
        | (((vfront >> 4) & 0x03) << 2) as u8
        | ((vsync >> 4) & 0x03) as u8;
    dtd[17] = 0x1e;
}

fn wait_for_connector_edid(card_name: &str, expected: &[u8]) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + SYSFS_TIMEOUT;
    let prefix = format!("{card_name}-");
    loop {
        for entry in std::fs::read_dir("/sys/class/drm").context("enumerate DRM sysfs")? {
            let entry = entry.context("read DRM sysfs entry")?;
            if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            let path = entry.path();
            if read_edid(&path).is_ok_and(|edid| edid == expected) {
                return Ok(path);
            }
        }
        if Instant::now() >= deadline {
            bail!("no {card_name} connector published the expected EDID");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_active_grant(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
) -> anyhow::Result<&'static str> {
    match client
        .client()
        .query_grant()
        .context("authoritatively query initial grant state")?
        .state
    {
        GrantState::Active => return Ok("initial-query"),
        GrantState::Revoked => bail!("grant was revoked before compositor activation"),
        _ => {}
    }

    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + GRANT_ACTIVE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let events = match tokio::time::timeout(remaining, client.read_events()).await {
                Ok(events) => events.context("read compositor grant-state event")?,
                Err(_) => {
                    let state = client
                        .client()
                        .query_grant()
                        .context("authoritatively query grant after activation timeout")?
                        .state;
                    if state == GrantState::Pending {
                        bail!(
                            "timed out waiting for compositor grant-state event; grant remains \
                             pending while the compositor may be deferring modesets for display \
                             power saving"
                        );
                    }
                    bail!(
                        "timed out waiting for compositor grant-state event; authoritative grant \
                         state is {state:?}"
                    );
                }
            };

            for event in events {
                match event {
                    CastKmsEvent::GrantState(_) => {
                        let state = client
                            .client()
                            .query_grant()
                            .context("authoritatively query grant after state event")?
                            .state;
                        match state {
                            GrantState::Active => return Ok("state-event"),
                            GrantState::Revoked => {
                                bail!("grant was revoked while waiting for compositor")
                            }
                            _ => {}
                        }
                    }
                    CastKmsEvent::GrantRevoked(event) => {
                        bail!(
                            "grant was revoked while waiting for compositor: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CaptureFrame(_)
                    | CastKmsEvent::CecTransmit(_)
                    | CastKmsEvent::Unknown(_) => {}
                }
            }
        }
    })
}

fn wait_for_capture_frame(
    runtime: &Runtime,
    client: &mut AsyncCastKmsClient,
    queue: CaptureQueue,
) -> anyhow::Result<CaptureFrameEvent> {
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + CAPTURE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let events = tokio::time::timeout(remaining, client.read_events())
                .await
                .context("timed out waiting for CastKMS capture frame")?
                .context("read CastKMS capture frame")?;

            for event in events {
                match event {
                    CastKmsEvent::CaptureFrame(event) => {
                        ensure!(
                            event.stream_id == queue.stream_id.get(),
                            "frame stream differs"
                        );
                        ensure!(
                            event.buffer_id == queue.buffer_id.get(),
                            "frame buffer differs"
                        );
                        ensure!(
                            event.user_data == queue.user_data.get(),
                            "frame user data differs"
                        );
                        return Ok(event);
                    }
                    CastKmsEvent::GrantState(_) => {
                        let state = client
                            .client()
                            .query_grant()
                            .context("query grant while awaiting capture")?
                            .state;
                        ensure!(
                            state == GrantState::Active,
                            "grant became {state:?} while awaiting capture"
                        );
                    }
                    CastKmsEvent::GrantRevoked(event) => {
                        bail!(
                            "grant was revoked while awaiting capture: status {}",
                            event.status
                        )
                    }
                    CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {}
                }
            }
        }
    })
}

fn validate_capture_frame(
    event: CaptureFrameEvent,
    mode_generation: u64,
    width: u32,
    height: u32,
    queue: CaptureQueue,
) -> anyhow::Result<()> {
    ensure!(
        event.stream_id == queue.stream_id.get(),
        "frame stream differs"
    );
    ensure!(
        event.buffer_id == queue.buffer_id.get(),
        "frame buffer differs"
    );
    ensure!(
        event.user_data == queue.user_data.get(),
        "frame user data differs"
    );
    ensure!(
        event.status == 0,
        "capture frame failed with {}",
        event.status
    );
    ensure!(
        event.mode_generation == mode_generation,
        "capture frame mode generation differs"
    );
    ensure!(
        event.flags & !CAPTURE_FRAME_FULL_DAMAGE == 0,
        "capture frame has unknown or mode-change flags 0x{:08x}",
        event.flags
    );
    ensure!(
        event.flags & CAPTURE_FRAME_MODE_CHANGED == 0,
        "capture mode changed during the frame"
    );
    ensure!(event.sequence > 0, "capture sequence is zero");
    ensure!(event.timestamp_ns > 0, "capture timestamp is not positive");
    ensure!(event.damage_x >= 0, "capture damage x is negative");
    ensure!(event.damage_y >= 0, "capture damage y is negative");
    ensure!(event.damage_width > 0, "capture damage width is zero");
    ensure!(event.damage_height > 0, "capture damage height is zero");
    let damage_right = u32::try_from(event.damage_x)
        .context("capture damage x conversion")?
        .checked_add(event.damage_width)
        .context("capture damage width overflow")?;
    let damage_bottom = u32::try_from(event.damage_y)
        .context("capture damage y conversion")?
        .checked_add(event.damage_height)
        .context("capture damage height overflow")?;
    ensure!(damage_right <= width, "capture damage exceeds frame width");
    ensure!(
        damage_bottom <= height,
        "capture damage exceeds frame height"
    );
    if event.flags & CAPTURE_FRAME_FULL_DAMAGE != 0 {
        ensure!(event.damage_x == 0, "full damage x is nonzero");
        ensure!(event.damage_y == 0, "full damage y is nonzero");
        ensure!(event.damage_width == width, "full damage width differs");
        ensure!(event.damage_height == height, "full damage height differs");
    }
    Ok(())
}

fn wait_for_edid(connector: &Path, expected: &[u8]) -> anyhow::Result<()> {
    wait_for_sysfs("EDID", || {
        Ok(read_edid(connector).is_ok_and(|edid| edid == expected))
    })
}

fn wait_for_status(connector: &Path, expected: &str) -> anyhow::Result<()> {
    wait_for_sysfs("connector status", || {
        let status = std::fs::read_to_string(connector.join("status"))?;
        Ok(status.trim() == expected)
    })
}

fn wait_for_sysfs(
    description: &str,
    mut predicate: impl FnMut() -> std::io::Result<bool>,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + SYSFS_TIMEOUT;
    loop {
        if predicate().with_context(|| format!("read {description} from DRM sysfs"))? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {description} in DRM sysfs");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_edid(connector: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(connector.join("edid"))
}
