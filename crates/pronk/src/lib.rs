mod caller;
mod capability_lease;
mod capture_health;
mod capture_output_layouts;
mod cast_display_slot;
mod dbus;
mod device_control_port;
mod device_recovery;
mod device_session;
mod device_session_port;
mod display;
mod display_media;
mod display_state;
mod drm_capture_pipeline;
mod gpu_output;
mod kernel_display;
mod kernel_display_port;
mod kernel_display_with_capture;
mod kernel_session;
mod kernel_session_provider;
mod manager;
mod media_driver;
mod media_pipeline_port;
mod media_policy;
mod media_remote;
mod media_session;
mod mutter_kernel_session;
mod preparation;
mod renderer_capture_pipeline;
mod renderer_session;
mod replaceable_device_session;
mod slot;

/// Entry points used by the daemon executable.
pub mod daemon {
    pub use crate::dbus::{emit_inventory_events, register_manager, serve_lifecycle_events};
    pub use crate::display::MediaRuntime;
    pub use crate::kernel_session_provider::KernelSessionProvider;
    pub use crate::manager::{BackendConfig, ManagerActor};
}

/// Integration fixture API. Production modules remain private to this crate.
#[doc(hidden)]
pub mod testing {
    pub mod caller {
        pub use crate::caller::{pin_bus_caller, query_bus_caller_credentials, BusCallerError};
    }
    pub mod dbus {
        pub use crate::dbus::{emit_inventory_events, register_manager, serve_lifecycle_events};
    }
    pub mod device_session {
        pub use crate::device_session::BackendDeviceSession;
    }
    pub mod device_session_port {
        pub use crate::device_session_port::{
            DeviceMediaConfiguration, DeviceMediaEndpoint, DeviceMediaKind, DeviceMediaSetup,
            DeviceMediaStopReason, DeviceMediaSuspendReason, DeviceMediaTarget, DeviceSessionPort,
            DeviceSessionStopReason,
        };
    }
    pub mod display {
        pub use crate::display::{DisplaySetupStage, MediaRuntime};
    }
    pub mod display_state {
        pub use crate::display_state::{MediaState, RouteTarget, RoutedMode};
    }
    pub mod drm_capture_pipeline {
        pub use crate::drm_capture_pipeline::{DrmCapturePipeline, DrmCapturePipelineConfig};
    }
    pub mod gpu_output {
        pub use crate::gpu_output::{GpuOutput, OutputEvent, OutputReady};
    }
    pub mod kernel_session {
        pub use crate::kernel_session::{KernelSession, KernelSessionError};
    }
    pub mod kernel_session_provider {
        pub use crate::kernel_session_provider::KernelSessionProvider;
    }
    pub mod manager {
        pub use crate::manager::{
            BackendConfig, InventoryEvent, ManagerActor, OutputInventoryProvider,
            OutputInventoryProviderError, SystemOutputInventoryProvider,
        };
    }
    pub mod media_pipeline_port {
        pub use crate::media_pipeline_port::{CaptureEvent, CaptureEventPort, CapturePipelinePort};
    }
    pub mod media_session {
        pub use crate::media_session::{MediaRoute, MediaStartRequest, MediaStopReason};
    }
    pub mod preparation {
        pub use crate::preparation::{initial_preparation_offer, PreparedCastDevice};
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use pronk_core::output::CastKmsOutput;
    use tokio_util::sync::CancellationToken;

    use crate::kernel_session::{KernelSession, KernelSessionError};
    use crate::kernel_session_provider::KernelSessionProvider;

    /// Unit-test dependency for paths that must never reach grant acquisition.
    #[derive(Debug)]
    pub struct UnreachableKernelSessionProvider;

    #[async_trait::async_trait]
    impl KernelSessionProvider for UnreachableKernelSessionProvider {
        async fn acquire(
            &self,
            _output: &CastKmsOutput,
            _audio_enabled: bool,
            _cancellation: CancellationToken,
        ) -> Result<KernelSession, KernelSessionError> {
            panic!("test unexpectedly reached kernel-session acquisition")
        }
    }
}
