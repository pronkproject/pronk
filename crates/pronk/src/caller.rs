//! Trusted session-bus caller identity for display-setup operations.

use nix::unistd::Uid;
use pronk_core::session::{CallerSessionError, PinnedCallerSession};
use thiserror::Error;
use zbus::names::UniqueName;
use zbus::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusCallerCredentials {
    pub pid: u32,
    pub uid: u32,
}

/// Resolve one immutable D-Bus unique name through the bus broker.
///
/// Both credentials come from one `GetConnectionCredentials` reply. Public
/// API input never supplies either value or a logind session identifier.
pub async fn query_bus_caller_credentials(
    connection: &Connection,
    sender: &UniqueName<'_>,
) -> Result<BusCallerCredentials, BusCallerError> {
    let proxy = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(BusCallerError::CreateBusProxy)?;
    let credentials = proxy
        .get_connection_credentials(sender.clone().into())
        .await
        .map_err(BusCallerError::QueryCredentials)?;
    let pid = credentials
        .process_id()
        .ok_or(BusCallerError::MissingProcessId)?;
    let uid = credentials
        .unix_user_id()
        .ok_or(BusCallerError::MissingUserId)?;
    if pid == 0 {
        return Err(BusCallerError::InvalidProcessId);
    }
    Ok(BusCallerCredentials { pid, uid })
}

/// Resolve, same-UID-check, and pidfd-pin the graphical login responsible for
/// one public session-bus method call.
pub async fn pin_bus_caller(
    connection: &Connection,
    sender: &UniqueName<'_>,
) -> Result<PinnedCallerSession, BusCallerError> {
    let credentials = query_bus_caller_credentials(connection, sender).await?;
    PinnedCallerSession::pin_async(credentials.pid, credentials.uid, Uid::effective().as_raw())
        .await
        .map_err(BusCallerError::PinSession)
}

#[derive(Debug, Error)]
pub enum BusCallerError {
    #[error("create session-bus broker proxy: {0}")]
    CreateBusProxy(zbus::Error),
    #[error("query session-bus caller credentials: {0}")]
    QueryCredentials(zbus::fdo::Error),
    #[error("session-bus broker omitted the caller process ID")]
    MissingProcessId,
    #[error("session-bus broker returned process ID zero")]
    InvalidProcessId,
    #[error("session-bus broker omitted the caller Unix user ID")]
    MissingUserId,
    #[error("pin caller process and graphical login session: {0}")]
    PinSession(#[source] CallerSessionError),
}
