use pronk_core::grant::{GrantAcquisitionError, GrantLease, GrantProvider, GrantTarget};
use tokio_util::sync::CancellationToken;

/// Integration-test dependency for paths that must not acquire a grant.
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
