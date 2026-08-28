//! Mutter-backed acquisition of normal CastKMS grants.
//!
//! Mutter retains the private kernel control endpoint. Pronk receives only the
//! restricted holder descriptor and therefore revokes a grant by dropping that
//! descriptor or disconnecting from the session bus.

use std::os::fd::OwnedFd;
use std::time::Duration;

use async_trait::async_trait;
use pronk_core::grant::{
    GrantAcquisitionError, GrantLease, GrantMetadata, GrantProvider, GrantTarget,
    GrantValidationError,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zbus::zvariant::OwnedFd as ZbusOwnedFd;

pub const MUTTER_CAST_KMS_BUS_NAME: &str = "org.gnome.Mutter.CastKms";
pub const MUTTER_CAST_KMS_PATH: &str = "/org/gnome/Mutter/CastKms";
const MUTTER_GRANT_TIMEOUT: Duration = Duration::from_secs(10);

#[zbus::proxy(
    interface = "org.gnome.Mutter.CastKms",
    default_service = "org.gnome.Mutter.CastKms",
    default_path = "/org/gnome/Mutter/CastKms",
    gen_blocking = false
)]
trait MutterCastKms {
    #[zbus(name = "CreateCaptureGrant")]
    fn create_capture_grant(
        &self,
        device_major: u32,
        device_minor: u32,
        connector_id: u32,
        profile: u16,
    ) -> zbus::Result<(ZbusOwnedFd, u32, u32, u32, u32, u32, u16, u16)>;
}

#[derive(Debug, Clone)]
pub struct MutterGrantProvider {
    client: MutterCastKmsClient,
    request_timeout: Duration,
}

#[derive(Debug, Clone)]
struct MutterCastKmsClient {
    connection: zbus::Connection,
}

#[derive(Debug)]
struct ReceivedGrant {
    holder: OwnedFd,
    metadata: GrantMetadata,
}

impl MutterGrantProvider {
    pub fn new(connection: zbus::Connection) -> Self {
        Self {
            client: MutterCastKmsClient { connection },
            request_timeout: MUTTER_GRANT_TIMEOUT,
        }
    }

    async fn acquire_inner(
        &self,
        target: &GrantTarget,
    ) -> Result<GrantLease, MutterGrantProviderError> {
        let received = self.client.request(target).await?;
        GrantLease::from_compositor(
            received.holder,
            received.metadata,
            target.connector_id,
            target.profile.rights(),
        )
        .map_err(MutterGrantProviderError::InvalidGrant)
    }
}

#[async_trait]
impl GrantProvider for MutterGrantProvider {
    async fn acquire(
        &self,
        target: GrantTarget,
        cancellation: CancellationToken,
    ) -> Result<GrantLease, GrantAcquisitionError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(GrantAcquisitionError::Cancelled),
            result = tokio::time::timeout(self.request_timeout, self.acquire_inner(&target)) => {
                match result {
                    Ok(result) => result.map_err(GrantAcquisitionError::provider),
                    Err(_) => Err(GrantAcquisitionError::provider(
                        MutterGrantProviderError::Timeout(self.request_timeout),
                    )),
                }
            }
        }
    }
}

impl MutterCastKmsClient {
    async fn request(
        &self,
        target: &GrantTarget,
    ) -> Result<ReceivedGrant, MutterGrantProviderError> {
        let proxy = MutterCastKmsProxy::new(&self.connection).await?;
        let (
            holder,
            grant_id,
            output_index,
            rights,
            flags,
            initial_state,
            capture_uapi_major,
            capture_uapi_minor,
        ) = proxy
            .create_capture_grant(
                target.device_major,
                target.device_minor,
                target.connector_id,
                target.profile as u16,
            )
            .await?;

        Ok(ReceivedGrant {
            holder: holder.into(),
            metadata: GrantMetadata {
                grant_id,
                connector_id: target.connector_id,
                output_index,
                rights,
                flags,
                initial_state,
                capture_uapi_major,
                capture_uapi_minor,
            },
        })
    }
}

#[derive(Debug, Error)]
pub enum MutterGrantProviderError {
    #[error("Mutter did not return a CastKMS grant within {0:?}")]
    Timeout(Duration),
    #[error("request a normal CastKMS grant from Mutter: {0}")]
    Dbus(#[from] zbus::Error),
    #[error("validate the normal CastKMS grant returned by Mutter: {0}")]
    InvalidGrant(#[source] GrantValidationError),
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use castkms_sys::{
        CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR, DISPLAY_CEC_V1_RIGHTS, GRANT_STATE_ACTIVE,
    };
    use pronk_core::grant::GrantProfile;
    use tokio::net::UnixStream;
    use tokio::sync::Notify;
    use zbus::connection::{AuthMechanism, Builder};
    use zbus::Guid;

    use super::*;

    type Request = (u32, u32, u32, u16);

    struct FakeMutter {
        holder: Mutex<Option<OwnedFd>>,
        requests: Arc<Mutex<Vec<Request>>>,
        release: Option<Arc<Notify>>,
    }

    #[zbus::interface(name = "org.gnome.Mutter.CastKms")]
    impl FakeMutter {
        #[zbus(name = "CreateCaptureGrant")]
        async fn create_capture_grant(
            &self,
            device_major: u32,
            device_minor: u32,
            connector_id: u32,
            profile: u16,
        ) -> zbus::fdo::Result<(ZbusOwnedFd, u32, u32, u32, u32, u32, u16, u16)> {
            self.requests
                .lock()
                .unwrap()
                .push((device_major, device_minor, connector_id, profile));
            if let Some(release) = &self.release {
                release.notified().await;
            }
            let holder = self
                .holder
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| zbus::fdo::Error::Failed("grant already requested".into()))?;
            Ok((
                holder.into(),
                91,
                3,
                DISPLAY_CEC_V1_RIGHTS,
                0,
                GRANT_STATE_ACTIVE,
                CAPTURE_UAPI_MAJOR,
                CAPTURE_UAPI_MINOR,
            ))
        }
    }

    fn target() -> GrantTarget {
        GrantTarget {
            device_major: 226,
            device_minor: 42,
            connector_id: 77,
            profile: GrantProfile::DisplayCecV1,
        }
    }

    async fn connections(fake: FakeMutter) -> (zbus::Connection, zbus::Connection) {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = Builder::unix_stream(server_stream)
            .server(Guid::generate())
            .unwrap()
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .serve_at(MUTTER_CAST_KMS_PATH, fake)
            .unwrap();
        let client = Builder::unix_stream(client_stream)
            .p2p()
            .auth_mechanism(AuthMechanism::External);
        tokio::try_join!(server.build(), client.build()).unwrap()
    }

    #[tokio::test]
    async fn dbus_client_sends_the_exact_target_and_receives_one_holder() {
        let (holder, mut holder_peer) = StdUnixStream::pair().unwrap();
        holder_peer
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (_server, client_connection) = connections(FakeMutter {
            holder: Mutex::new(Some(OwnedFd::from(holder))),
            requests: Arc::clone(&requests),
            release: None,
        })
        .await;
        let client = MutterCastKmsClient {
            connection: client_connection,
        };

        let received = client.request(&target()).await.unwrap();
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[(226, 42, 77, GrantProfile::DisplayCecV1 as u16)]
        );
        assert_eq!(
            received.metadata,
            GrantMetadata {
                grant_id: 91,
                connector_id: 77,
                output_index: 3,
                rights: DISPLAY_CEC_V1_RIGHTS,
                flags: 0,
                initial_state: GRANT_STATE_ACTIVE,
                capture_uapi_major: CAPTURE_UAPI_MAJOR,
                capture_uapi_minor: CAPTURE_UAPI_MINOR,
            }
        );

        drop(received);
        let mut byte = [0_u8; 1];
        assert_eq!(holder_peer.read(&mut byte).unwrap(), 0);
    }

    #[tokio::test]
    async fn cancelled_request_never_reaches_mutter() {
        let (holder, _holder_peer) = StdUnixStream::pair().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (_server, client_connection) = connections(FakeMutter {
            holder: Mutex::new(Some(OwnedFd::from(holder))),
            requests: Arc::clone(&requests),
            release: None,
        })
        .await;
        let provider = MutterGrantProvider::new(client_connection);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            provider.acquire(target(), cancellation).await,
            Err(GrantAcquisitionError::Cancelled)
        ));
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stalled_mutter_request_fails_with_a_bounded_timeout() {
        let (holder, _holder_peer) = StdUnixStream::pair().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (_server, client_connection) = connections(FakeMutter {
            holder: Mutex::new(Some(OwnedFd::from(holder))),
            requests: Arc::clone(&requests),
            release: Some(Arc::new(Notify::new())),
        })
        .await;
        let mut provider = MutterGrantProvider::new(client_connection);
        provider.request_timeout = Duration::from_millis(100);

        let error = provider
            .acquire(target(), CancellationToken::new())
            .await
            .unwrap_err();
        let GrantAcquisitionError::Provider(error) = error else {
            panic!("stalled request was not reported as a provider failure");
        };
        assert!(matches!(
            error.downcast_ref::<MutterGrantProviderError>(),
            Some(MutterGrantProviderError::Timeout(timeout))
                if *timeout == Duration::from_millis(100)
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
