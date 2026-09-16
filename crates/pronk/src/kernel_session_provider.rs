//! Application boundary for acquiring one authorized kernel display lifetime.

use async_trait::async_trait;
use pronk_core::output::CastKmsOutput;
use tokio_util::sync::CancellationToken;

use crate::kernel_session::{KernelSession, KernelSessionError};

#[async_trait]
pub trait KernelSessionProvider: std::fmt::Debug + Send + Sync + 'static {
    /// Return whether acquiring the output is worth attempting.
    ///
    /// The result is only a preliminary selection rule. The provider must
    /// still arbitrate ownership when `acquire` runs.
    fn may_acquire(&self, output: &CastKmsOutput) -> bool {
        output.is_available()
    }

    /// Acquire separate capabilities under one issuer-owned lifetime.
    ///
    /// Dropping or cancelling the future must still clean up late-issued
    /// authority. Providers bound pending acquisition and retiring sessions.
    async fn acquire(
        &self,
        output: &CastKmsOutput,
        audio_enabled: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError>;
}
