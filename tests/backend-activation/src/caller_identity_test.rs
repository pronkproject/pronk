use std::os::unix::fs::MetadataExt;

use anyhow::{ensure, Context};
use pronk::caller::{pin_bus_caller, query_bus_caller_credentials, BusCallerError};
use pronk_core::session::CallerSessionError;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("connect to session bus")?;
    let sender = connection
        .unique_name()
        .context("session bus did not assign a unique name")?;
    let credentials = query_bus_caller_credentials(&connection, sender).await?;
    ensure!(
        credentials.pid == std::process::id(),
        "broker returned PID {} for test process {}",
        credentials.pid,
        std::process::id()
    );
    let uid = std::fs::metadata("/proc/self")?.uid();
    ensure!(credentials.uid == uid, "broker returned the wrong UID");
    println!("dbus_broker_caller_credentials=pass");

    match pin_bus_caller(&connection, sender).await {
        Ok(caller) => {
            ensure!(caller.pid() == credentials.pid, "pinned PID differs");
            ensure!(caller.uid() == credentials.uid, "pinned UID differs");
            ensure!(!caller.session_id().is_empty(), "empty logind session ID");
            ensure!(!caller.seat().is_empty(), "empty graphical seat");
            println!("caller_pidfd_identity=pass");
            println!("caller_graphical_session={}", caller.session_id());
        }
        Err(BusCallerError::PinSession(CallerSessionError::NoGraphicalSession)) => {
            println!("caller_graphical_session=skip:no-active-local-graphical-session");
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
