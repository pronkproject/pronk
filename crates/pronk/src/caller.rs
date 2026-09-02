//! Trusted D-Bus caller identity for public service operations.

use nix::unistd::Uid;
use pronk_core::session::{CallerSessionError, PinnedCallerProcess, PinnedCallerSession};
use thiserror::Error;
use zbus::names::UniqueName;
use zbus::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusCallerCredentials {
    pub pid: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicBus {
    Session,
    System,
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

pub async fn pin_bus_caller_for(
    connection: &Connection,
    sender: &UniqueName<'_>,
    bus: PublicBus,
) -> Result<PinnedCallerProcess, BusCallerError> {
    let credentials = query_bus_caller_credentials(connection, sender).await?;
    match bus {
        PublicBus::Session => PinnedCallerSession::pin_async(
            credentials.pid,
            credentials.uid,
            Uid::effective().as_raw(),
        )
        .await
        .map(PinnedCallerSession::into_process),
        PublicBus::System => {
            PinnedCallerProcess::pin_async(
                credentials.pid,
                credentials.uid,
                Uid::effective().as_raw(),
            )
            .await
        }
    }
    .map_err(BusCallerError::PinSession)
}

/// Pin a system-bus caller against the credentials assigned by the bus.
///
/// This is used only after a separate authorization decision has accepted an
/// ordinary desktop user. Re-reading `/proc` through the pidfd-backed helper
/// prevents a recycled process ID or changed process identity from inheriting
/// that decision.
pub async fn pin_authorized_system_bus_caller(
    credentials: BusCallerCredentials,
) -> Result<PinnedCallerProcess, BusCallerError> {
    PinnedCallerProcess::pin_async(credentials.pid, credentials.uid, credentials.uid)
        .await
        .map_err(BusCallerError::PinSession)
}

#[derive(Debug, Error)]
pub enum BusCallerError {
    #[error("create bus-broker proxy: {0}")]
    CreateBusProxy(zbus::Error),
    #[error("query D-Bus caller credentials: {0}")]
    QueryCredentials(zbus::fdo::Error),
    #[error("bus broker omitted the caller process ID")]
    MissingProcessId,
    #[error("bus broker returned process ID zero")]
    InvalidProcessId,
    #[error("bus broker omitted the caller Unix user ID")]
    MissingUserId,
    #[error("pin caller process identity: {0}")]
    PinSession(#[source] CallerSessionError),
}
