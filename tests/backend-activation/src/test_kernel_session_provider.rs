use pronk::kernel_session::{KernelSession, KernelSessionError};
use pronk::kernel_session_provider::KernelSessionProvider;
use pronk_core::output::CastKmsOutput;
use tokio_util::sync::CancellationToken;

/// Integration-test dependency for paths that must not acquire a kernel session.
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
