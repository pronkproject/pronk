use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use pronk::dbus::{emit_inventory_events, register_manager, serve_lifecycle_events};
use pronk::manager::{BackendConfig, ManagerActor};
use pronk_backend_host::{BackendEndpoint, BackendReconnectPolicy, ExactRegistrationValidator};
use pronk_dbus::BUS_NAME;
use tokio::time::{sleep, timeout};

mod test_grant_provider;

use test_grant_provider::UnreachableGrantProvider;

const START_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket = parse_socket()?;
    let endpoint = BackendEndpoint::new("mock", socket, "pronk-backend-mock@.service")?;
    let validator = Arc::new(ExactRegistrationValidator::new("mock", "development"));
    let policy =
        BackendReconnectPolicy::new(0, Duration::ZERO, Duration::ZERO, Duration::from_secs(1))?;
    let mut manager = ManagerActor::spawn(
        vec![BackendConfig::new(endpoint, 501, validator, policy)],
        Arc::new(UnreachableGrantProvider),
    )?;
    let connection = zbus::Connection::session()
        .await
        .context("connect to isolated session bus")?;
    register_manager(&connection, manager.handle()).await?;
    let inventory_events = manager
        .take_events()
        .context("manager event stream was already taken")?;
    let lifecycle_events = manager
        .take_lifecycle_events()
        .context("manager lifecycle event stream was already taken")?;
    let signal_connection = connection.clone();
    let signal_task =
        tokio::spawn(
            async move { emit_inventory_events(&signal_connection, inventory_events).await },
        );
    let lifecycle_connection = connection.clone();
    let lifecycle_manager = manager.handle();
    let lifecycle_task = tokio::spawn(async move {
        serve_lifecycle_events(&lifecycle_connection, lifecycle_manager, lifecycle_events).await
    });

    timeout(START_TIMEOUT, async {
        loop {
            if manager.handle().list_devices().await?.devices.len() == 2 {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("mock devices did not enter the public inventory")??;
    connection.request_name(BUS_NAME).await?;

    wait_for_termination_signal().await?;
    let _ = connection.release_name(BUS_NAME).await;
    let report = manager.shutdown().await?;
    anyhow::ensure!(
        report.errors.is_empty(),
        "manager shutdown failed: {report:?}"
    );
    signal_task.await??;
    lifecycle_task.await??;
    Ok(())
}

fn parse_socket() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let socket = args.next().context("missing backend socket path")?;
    anyhow::ensure!(args.next().is_none(), "unexpected extra argument");
    Ok(socket.into())
}

async fn wait_for_termination_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}
