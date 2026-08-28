use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_backend_host::{
    BackendEndpoint, BackendReconnectPolicy, BackendSupervisor, BackendSupervisorEvent,
    SystemdRegistrationValidator,
};
use tokio::time::timeout;

const GATE_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let socket_path = parse_socket_path()?;
    let endpoint = BackendEndpoint::new("mock", socket_path, "pronk-backend-mock@.service")?;
    let validator = Arc::new(
        SystemdRegistrationValidator::session()
            .await
            .context("connect to the user systemd manager")?,
    );
    let mut supervisor = BackendSupervisor::spawn(
        endpoint,
        1,
        validator,
        BackendReconnectPolicy::new(0, Duration::ZERO, Duration::ZERO, Duration::from_secs(30))?,
    )?;
    ensure!(
        next_event(&mut supervisor).await?
            == BackendSupervisorEvent::Connecting {
                connection_generation: 1,
            },
        "user-manager supervisor did not begin generation 1"
    );
    match next_event(&mut supervisor).await? {
        BackendSupervisorEvent::Connected {
            connection_generation,
            inventory,
            ..
        } => {
            ensure!(
                connection_generation == 1,
                "unexpected connection generation"
            );
            ensure!(
                inventory.devices.len() == 2,
                "mock user-manager instance returned unexpected inventory"
            );
        }
        event => anyhow::bail!("unexpected user-manager supervisor event: {event:?}"),
    }
    let report = timeout(GATE_TIMEOUT, supervisor.shutdown())
        .await
        .context("user-manager supervisor did not stop")??;
    ensure!(report.graceful, "user-manager shutdown failed: {report:?}");

    println!("systemd_invocation_id=pass");
    println!("systemd_template_instance=pass");
    println!("systemd_socket_trigger=pass");
    println!("systemd_main_pid=pass");
    println!("systemd_notify_lifecycle=pass");
    println!("systemd_backend_supervisor=pass");
    println!("systemd_core_peer_identity=pass");
    Ok(())
}

async fn next_event(supervisor: &mut BackendSupervisor) -> anyhow::Result<BackendSupervisorEvent> {
    timeout(GATE_TIMEOUT, supervisor.next_event())
        .await
        .context("user-manager supervisor event timed out")?
        .context("user-manager supervisor event stream closed")
}

fn parse_socket_path() -> anyhow::Result<PathBuf> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let socket_path = arguments.next().context("missing backend socket path")?;
    ensure!(arguments.next().is_none(), "unexpected extra argument");
    Ok(socket_path)
}
