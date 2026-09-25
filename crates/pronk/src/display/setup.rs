use std::future::Future;
use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::sync::Arc;

use nix::unistd::Uid;
use pronk_backend_host::BackendSessionHandle;
use pronk_backend_protocol::{PreparationRequest, StopReason};
use pronk_core::edid::{EdidMode, ValidatedEdid};
use pronk_core::identity::PnpIdResolver;
use pronk_core::session::PinnedCallerProcess;
use pronk_dbus::{DeviceInfo, OperationErrorCode};
use pronk_pipewire::{ClassifiedSocketPaths, ClassifiedSocketRemoteProvider, VideoFrameRate};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{
    set_status, AddedCastDisplay, AddedCastDisplayResources, CastDisplayId, DisplaySetupError,
    DisplaySetupSnapshot, DisplaySetupStage, MediaRuntime, PendingDisplaySelection,
    INITIAL_SESSION_GENERATION, VIDEO_FRAME_RATE_DENOMINATOR, VIDEO_FRAME_RATE_NUMERATOR,
};
use crate::device_recovery::{
    DeviceSessionFactoryError, DeviceSessionFactoryPort, PreparedDeviceSession,
};
use crate::device_session::{BackendDeviceSession, BackendDeviceSessionEvents};
use crate::display_media::{CaptureSource, DisplayMediaAccess, DisplayMediaConfig};
use crate::kernel_display::{
    AttachError, KernelDisplay, KernelDisplayConfig, DEFAULT_TOPOLOGY_POLL_INTERVAL,
};
use crate::kernel_display_port::KernelDisplayPort;
use crate::kernel_display_with_capture::KernelDisplayWithCapture;
use crate::kernel_session::{KernelSession, KernelSessionError};
use crate::kernel_session_provider::KernelSessionProvider;
use crate::manager::{
    CastDisplaySlotLease, DeviceSessionResolver, ManagerHandle, ReservedCastDisplaySlot,
};
use crate::media_driver::ProductionMediaSessionDriver;
use crate::media_remote::ClassifiedDeviceMediaRemotePort;
use crate::preparation::PreparedCastDevice;
use crate::replaceable_device_session::replaceable_device_session;

const KERNEL_AUDIO_ENABLED: bool = false;

pub(super) struct DisplaySetupContext {
    pub(super) display_id: CastDisplayId,
    pub(super) kernel_session_provider: Arc<dyn KernelSessionProvider>,
    pub(super) pnp_resolver: Arc<PnpIdResolver>,
    pub(super) media_runtime: MediaRuntime,
    pub(super) offer: PreparationRequest,
    pub(super) audio_enabled: bool,
    pub(super) cancellation: CancellationToken,
    pub(super) status: watch::Sender<DisplaySetupSnapshot>,
}

pub(super) struct AttachedKernelSession {
    pub(super) kernel: KernelDisplay,
    pub(super) media: DisplayMediaAccess,
}

fn offer_for_kernel_session(offer: &PreparationRequest) -> PreparationRequest {
    let mut offer = offer.clone();
    offer.audio_profiles.clear();
    offer.requested_features = 0;
    offer
}

pub(super) async fn attach_kernel_session(
    session: KernelSession,
    edid: ValidatedEdid,
    crtc_id: NonZeroU32,
    modes: Vec<EdidMode>,
    capture_source: CaptureSource,
    cancellation: &CancellationToken,
) -> Result<AttachedKernelSession, DisplaySetupError> {
    let mut kernel = KernelDisplay::attach(
        session,
        Some(edid),
        KernelDisplayConfig {
            crtc_id,
            modes,
            poll_interval: DEFAULT_TOPOLOGY_POLL_INTERVAL,
        },
        cancellation.clone(),
    )
    .await
    .map_err(|error| match error {
        AttachError::Cancelled => DisplaySetupError::Cancelled,
        error => DisplaySetupError::KernelAttach(error),
    })?;
    let media = match capture_source {
        CaptureSource::Renderer => kernel.capture_access().and_then(|capture| {
            kernel
                .take_renderer_access()
                .map(|renderer| DisplayMediaAccess::Renderer { renderer, capture })
        }),
        CaptureSource::FinalImage => kernel.capture_access().map(DisplayMediaAccess::FinalImage),
    };
    let media = match media {
        Ok(media) => media,
        Err(error) => {
            cleanup_kernel_display(kernel).await;
            return Err(DisplaySetupError::KernelAccess(error));
        }
    };
    Ok(AttachedKernelSession { kernel, media })
}

pub(super) async fn cleanup_attached_kernel_session(session: AttachedKernelSession) {
    if let Err(error) = session.media.release().await {
        warn!(%error, "display setup could not release its unused media authority");
    }
    cleanup_kernel_display(session.kernel).await;
}

async fn cleanup_kernel_display(kernel: KernelDisplay) {
    let result = Box::new(kernel)
        .detach()
        .await
        .map_err(|error| error.to_string());
    if let Err(error) = result {
        warn!(%error, "display setup could not release its attached kernel session");
    }
}

pub(super) enum DisplayReservation {
    #[cfg(test)]
    Ready(Box<ReservedCastDisplaySlot>),
    Pending {
        manager: ManagerHandle,
        pending: PendingDisplaySelection,
    },
}

pub(super) struct DisplaySetupCaller {
    pub(super) exit: Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>>,
}

impl From<PinnedCallerProcess> for DisplaySetupCaller {
    fn from(caller: PinnedCallerProcess) -> Self {
        let exit = Box::pin(async move { caller.wait_for_exit().await });
        Self { exit }
    }
}

pub(super) async fn run_display_setup(
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
        #[cfg(test)]
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

/// A reserved output with its matching kernel session acquired and revalidated.
struct KernelReadySetup {
    slot: ReservedCastDisplaySlot,
    context: DisplaySetupContext,
    device: DeviceInfo,
    output: pronk_core::output::CastKmsOutput,
    kernel_session: KernelSession,
    offer: PreparationRequest,
}

/// Backend preparation is complete, but the kernel display is not yet attached.
struct BackendReadySetup {
    slot: CastDisplaySlotLease,
    context: DisplaySetupContext,
    device: DeviceInfo,
    output: pronk_core::output::CastKmsOutput,
    kernel_session: KernelSession,
    offer: PreparationRequest,
    backend_session: BackendSessionHandle,
    prepared: PreparedCastDevice,
    video_profile_id: String,
    raw_layout: pronk_backend_protocol::RawVideoLayout,
    session_resolver: DeviceSessionResolver,
    session_events: BackendDeviceSessionEvents,
}

/// Kernel attachment and capture authority are ready for media construction.
struct AttachedSetup {
    slot: CastDisplaySlotLease,
    context: DisplaySetupContext,
    device: DeviceInfo,
    output: pronk_core::output::CastKmsOutput,
    offer: PreparationRequest,
    backend_session: BackendSessionHandle,
    prepared: PreparedCastDevice,
    video_profile_id: String,
    raw_layout: pronk_backend_protocol::RawVideoLayout,
    session_resolver: DeviceSessionResolver,
    session_events: BackendDeviceSessionEvents,
    attached: AttachedKernelSession,
}

async fn run_display_setup_inner(
    slot: ReservedCastDisplaySlot,
    context: DisplaySetupContext,
) -> Result<AddedCastDisplay, DisplaySetupError> {
    KernelReadySetup::acquire(slot, context)
        .await?
        .prepare_backend()
        .await?
        .attach()
        .await?
        .finish()
        .await
}

impl KernelReadySetup {
    async fn acquire(
        slot: ReservedCastDisplaySlot,
        context: DisplaySetupContext,
    ) -> Result<Self, DisplaySetupError> {
        if context.cancellation.is_cancelled() {
            return Err(DisplaySetupError::Cancelled);
        }
        let device = slot.device().clone();
        let output = slot.output().clone();
        let kernel_session = context
            .kernel_session_provider
            .acquire(&output, context.audio_enabled, context.cancellation.clone())
            .await
            .map_err(|error| match error {
                KernelSessionError::Cancelled => DisplaySetupError::Cancelled,
                error => DisplaySetupError::KernelSession(error),
            })?;
        let mut offer = offer_for_kernel_session(&context.offer);
        if context.media_runtime.capture_source == CaptureSource::Renderer {
            if let Some(render_node) = kernel_session.renderer_render_node() {
                let render_node = render_node.to_path_buf();
                let probe_modes = offer.candidate_modes.clone();
                let mut probe = tokio::task::spawn_blocking(move || {
                    crate::capture_output_layouts::for_modes(&render_node, &probe_modes)
                });
                let result = tokio::select! {
                    biased;
                    _ = context.cancellation.cancelled() => return Err(DisplaySetupError::Cancelled),
                    result = &mut probe => result,
                };
                let layouts = match result {
                    Ok(Ok(layouts)) => layouts,
                    Ok(Err(error)) => {
                        warn!(%error, "renderer output layout probe failed");
                        crate::capture_output_layouts::system_only_offer()
                    }
                    Err(error) => {
                        warn!(%error, "renderer output layout worker failed");
                        crate::capture_output_layouts::system_only_offer()
                    }
                };
                offer.video_profiles[0].raw_layouts = layouts.raw_layouts;
                offer.mode_raw_layouts = layouts.mode_raw_layouts;
            } else {
                offer.video_profiles[0].raw_layouts = crate::capture_output_layouts::system_only();
            }
        }
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
        Ok(Self {
            slot,
            context,
            device,
            output,
            kernel_session,
            offer,
        })
    }

    async fn prepare_backend(self) -> Result<BackendReadySetup, DisplaySetupError> {
        let Self {
            slot,
            context,
            device,
            output,
            kernel_session,
            offer,
        } = self;
        set_status(
            &context.status,
            DisplaySetupStage::PreparingDevice,
            OperationErrorCode::None,
            None,
        );
        let (slot, selection) = slot.into_lease();
        let mut create_session = Box::pin(selection.create_session(
            context.display_id.to_string(),
            INITIAL_SESSION_GENERATION,
            offer.requested_features,
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

        let mut prepare = Box::pin(backend_session.prepare(offer.clone()));
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
            KERNEL_AUDIO_ENABLED,
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
        let video_profile = &prepared.capabilities().video_profiles[0];
        let video_profile_id = video_profile.profile_id.clone();
        let raw_layout = match crate::display_media::select_raw_layout(
            &offer.video_profiles[0].raw_layouts,
            &video_profile.raw_layouts,
        ) {
            Ok(raw_layout) => raw_layout,
            Err(error) => {
                stop_partial_backend(backend_session).await;
                return Err(DisplaySetupError::Monitor(error.to_string()));
            }
        };
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
        let session_resolver = slot.device_session_resolver();
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

        Ok(BackendReadySetup {
            slot,
            context,
            device,
            output,
            kernel_session,
            offer,
            backend_session,
            prepared,
            video_profile_id,
            raw_layout,
            session_resolver,
            session_events,
        })
    }
}

impl BackendReadySetup {
    async fn attach(self) -> Result<AttachedSetup, DisplaySetupError> {
        let Self {
            slot,
            context,
            device,
            output,
            kernel_session,
            offer,
            backend_session,
            prepared,
            video_profile_id,
            raw_layout,
            session_resolver,
            session_events,
        } = self;
        set_status(
            &context.status,
            DisplaySetupStage::Attaching,
            OperationErrorCode::None,
            None,
        );
        let edid = prepared.generated_edid().edid().clone();
        let crtc_id = match NonZeroU32::new(output.crtc_id) {
            Some(crtc_id) => crtc_id,
            None => {
                stop_partial_backend(backend_session).await;
                return Err(DisplaySetupError::Monitor(
                    "reserved CastKMS output has a zero CRTC ID".into(),
                ));
            }
        };
        let modes = prepared.generated_edid().modes().to_vec();
        let attached = match attach_kernel_session(
            kernel_session,
            edid,
            crtc_id,
            modes,
            context.media_runtime.capture_source,
            &context.cancellation,
        )
        .await
        {
            Ok(attached) => attached,
            Err(error) => {
                stop_partial_backend(backend_session).await;
                return Err(error);
            }
        };
        if context.cancellation.is_cancelled() {
            stop_partial_backend(backend_session).await;
            cleanup_attached_kernel_session(attached).await;
            return Err(DisplaySetupError::Cancelled);
        }

        Ok(AttachedSetup {
            slot,
            context,
            device,
            output,
            offer,
            backend_session,
            prepared,
            video_profile_id,
            raw_layout,
            session_resolver,
            session_events,
            attached,
        })
    }
}

impl AttachedSetup {
    async fn finish(self) -> Result<AddedCastDisplay, DisplaySetupError> {
        let Self {
            slot,
            context,
            device,
            output,
            offer,
            backend_session,
            prepared,
            video_profile_id,
            raw_layout,
            session_resolver,
            session_events,
            attached,
        } = self;
        let pipewire_paths =
            match ClassifiedSocketPaths::in_runtime_dir(context.media_runtime.directory.clone()) {
                Ok(paths) => paths,
                Err(error) => {
                    stop_partial_backend(backend_session).await;
                    cleanup_attached_kernel_session(attached).await;
                    return Err(DisplaySetupError::Monitor(format!(
                        "construct classified PipeWire paths: {error}"
                    )));
                }
            };
        let remote_provider = ClassifiedSocketRemoteProvider::new_for_server_uid(
            pipewire_paths,
            Uid::from_raw(context.media_runtime.server_uid),
        );
        let AttachedKernelSession { kernel, media } = attached;
        let session_id = context.display_id.to_string();
        let initial_session_generation = NonZeroU64::new(INITIAL_SESSION_GENERATION)
            .expect("initial session generation is nonzero");
        let device_instance = format!("cast-display-{}", context.display_id.object_segment());
        let video_bitrate = NonZeroU64::new(8_000_000).expect("fixed bitrate is nonzero");
        let pipeline = media.create_pipeline(
            remote_provider.clone(),
            DisplayMediaConfig {
                connector_id: NonZeroU32::new(output.connector_id)
                    .expect("reserved CastKMS outputs have nonzero connector IDs"),
                output_index: output.id.output_index,
                session_id: session_id.clone(),
                device_instance: device_instance.clone(),
                node_description: device.display_name.clone(),
                video_profile_id: video_profile_id.clone(),
                raw_layout,
                video_bitrate,
                video_frame_rate: VideoFrameRate::new(
                    NonZeroU32::new(VIDEO_FRAME_RATE_NUMERATOR)
                        .expect("fixed frame-rate numerator is nonzero"),
                    NonZeroU32::new(VIDEO_FRAME_RATE_DENOMINATOR)
                        .expect("fixed frame-rate denominator is nonzero"),
                ),
            },
        );
        let (capture, capture_events) = match pipeline {
            Ok(pipeline) => pipeline,
            Err(error) => {
                stop_partial_backend(backend_session).await;
                cleanup_kernel_display(kernel).await;
                return Err(DisplaySetupError::Monitor(format!(
                    "open selected capture pipeline: {error}"
                )));
            }
        };
        let (device_session, _device_control, session_replacement) = replaceable_device_session(
            initial_session_generation,
            Box::new(BackendDeviceSession::new(backend_session)),
        );
        let kernel = Box::new(KernelDisplayWithCapture::new(kernel, capture_events))
            as Box<dyn KernelDisplayPort>;
        let remote_port = ClassifiedDeviceMediaRemotePort::new(
            remote_provider,
            session_id,
            device.backend_id.clone(),
        );
        let media_driver =
            ProductionMediaSessionDriver::new(capture, Box::new(remote_port), device_session);
        let recovery_factory = BackendPreparedDeviceSessionFactory {
            resolver: session_resolver,
            session_id: context.display_id.to_string(),
            offer,
            pnp_resolver: Arc::clone(&context.pnp_resolver),
            audio_enabled: KERNEL_AUDIO_ENABLED,
        };
        let state_revision = device.device_revision;
        Ok(AddedCastDisplay {
            resources: Some(AddedCastDisplayResources {
                display_id: context.display_id,
                state_revision,
                device,
                prepared,
                slot,
                media_driver: Box::new(media_driver),
                recovery_factory: Box::new(recovery_factory),
                session_replacement,
                initial_session_generation,
                session_events: Box::new(session_events),
                kernel,
            }),
        })
    }
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
