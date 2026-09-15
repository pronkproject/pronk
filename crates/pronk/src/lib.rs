pub mod brokered_kernel_display;
pub mod caller;
pub mod cast_display_slot;
pub mod castkms_actor;
pub mod cec_bridge;
pub mod dbus;
pub mod device_control_port;
pub mod device_recovery;
pub mod device_session;
pub mod device_session_port;
pub mod display;
pub mod display_state;
pub mod drm_capture_pipeline;
pub mod gpu_output;
pub mod kernel_display;
pub mod kernel_display_port;
pub mod kernel_session_provider;
pub mod manager;
pub mod media_driver;
pub mod media_pipeline_port;
pub mod media_policy;
pub mod media_remote;
pub mod media_session;
pub mod mutter_grant_provider;
pub mod preparation;
pub mod renderer_capture_pipeline;
pub mod replaceable_device_session;
mod slot;
pub mod system_authorization;

#[cfg(test)]
pub(crate) mod test_support {
    use pronk_core::output::CastKmsOutput;
    use tokio_util::sync::CancellationToken;

    use crate::kernel_session_provider::{
        KernelSession, KernelSessionError, KernelSessionProvider,
    };

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
