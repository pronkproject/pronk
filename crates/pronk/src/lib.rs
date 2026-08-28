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
pub mod kernel_display;
pub mod kernel_display_port;
pub mod manager;
pub mod media_driver;
pub mod media_pipeline_port;
pub mod media_policy;
pub mod media_remote;
pub mod media_session;
pub mod mutter_grant_provider;
pub mod preparation;
pub mod replaceable_device_session;
mod slot;

#[cfg(test)]
pub(crate) mod test_support {
    use pronk_core::grant::{GrantAcquisitionError, GrantLease, GrantProvider, GrantTarget};
    use tokio_util::sync::CancellationToken;

    /// Unit-test dependency for paths that must never reach grant acquisition.
    #[derive(Debug)]
    pub struct UnreachableGrantProvider;

    #[async_trait::async_trait]
    impl GrantProvider for UnreachableGrantProvider {
        async fn acquire(
            &self,
            _target: GrantTarget,
            _cancellation: CancellationToken,
        ) -> Result<GrantLease, GrantAcquisitionError> {
            panic!("test unexpectedly reached grant acquisition")
        }
    }
}
