use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use nix::unistd::Uid;
use pronk::dbus::{emit_inventory_events, register_manager, serve_lifecycle_events};
use pronk::manager::{BackendConfig, ManagerActor};
use pronk::mutter_grant_provider::MutterGrantProvider;
use pronk_backend_host::{
    BackendReconnectPolicy, BackendRegistrationValidator, BackendRegistry,
    SystemdRegistrationValidator,
};
use pronk_dbus::BUS_NAME;
use tokio::runtime::Builder;
use tracing::{info, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

fn main() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--version")) {
        println!("pronk {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args_os().len() != 1 {
        anyhow::bail!("usage: pronkd");
    }

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize logging: {error}"))?;

    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?;
    runtime.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let runtime_directory = PathBuf::from(format!("/run/user/{}", Uid::effective().as_raw()));
    let registry = BackendRegistry::load_installed(&runtime_directory)
        .context("load the installed backend registry")?;
    let connection = zbus::Connection::session()
        .await
        .context("connect to the session bus")?;
    let validator: Arc<dyn BackendRegistrationValidator> =
        Arc::new(SystemdRegistrationValidator::new(connection.clone()));
    let configs = registry
        .iter()
        .map(|(_, backend)| {
            BackendConfig::new(
                backend.endpoint().clone(),
                1,
                validator.clone(),
                BackendReconnectPolicy::default(),
            )
        })
        .collect();
    // Mutter authorizes each request by the sender that owns Pronk's public
    // bus name, so grant calls must use this same session-bus connection.
    let grant_provider = Arc::new(MutterGrantProvider::new(connection.clone()));
    let mut manager =
        ManagerActor::spawn(configs, grant_provider).context("start the Pronk manager")?;
    register_manager(&connection, manager.handle())
        .await
        .context("register the public manager object")?;
    let inventory_events = manager
        .take_events()
        .context("manager inventory events were already claimed")?;
    let lifecycle_events = manager
        .take_lifecycle_events()
        .context("manager lifecycle events were already claimed")?;
    let signal_connection = connection.clone();
    let mut signal_task =
        tokio::spawn(
            async move { emit_inventory_events(&signal_connection, inventory_events).await },
        );
    let lifecycle_connection = connection.clone();
    let lifecycle_manager = manager.handle();
    let mut lifecycle_task = tokio::spawn(async move {
        serve_lifecycle_events(&lifecycle_connection, lifecycle_manager, lifecycle_events).await
    });

    connection
        .request_name(BUS_NAME)
        .await
        .context("acquire the Pronk session-bus name")?;
    info!(
        backends = registry.len(),
        "Pronk device inventory is available"
    );

    let ended_task = tokio::select! {
        result = wait_for_termination_signal() => {
            result.context("wait for termination signal")?;
            None
        }
        result = &mut signal_task => {
            result.context("join inventory signal task")??;
            Some("inventory signal")
        }
        result = &mut lifecycle_task => {
            result.context("join lifecycle signal task")??;
            Some("lifecycle signal")
        }
    };

    let report = manager.shutdown().await.context("stop the Pronk manager")?;
    for (backend_id, error) in &report.errors {
        warn!(backend_id, error, "backend did not shut down cleanly");
    }
    if ended_task != Some("inventory signal") {
        signal_task.await.context("join inventory signal task")??;
    }
    if ended_task != Some("lifecycle signal") {
        lifecycle_task
            .await
            .context("join lifecycle signal task")??;
    }
    if let Err(error) = connection.release_name(BUS_NAME).await {
        warn!(%error, "failed to release the Pronk session-bus name");
    }
    if let Some(task) = ended_task {
        anyhow::bail!("{task} task stopped unexpectedly");
    }
    info!("Pronk stopped");
    Ok(())
}

async fn wait_for_termination_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
