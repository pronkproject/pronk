use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use nix::libc;
use nix::unistd::{getresgid, getresuid, Uid, User};
use pronk::caller::PublicBus;
use pronk::dbus::{emit_inventory_events, register_manager, serve_lifecycle_events};
use pronk::display::MediaRuntime;
use pronk::manager::{BackendConfig, ManagerActor};
use pronk::mutter_grant_provider::MutterGrantProvider;
use pronk_backend_host::{
    BackendReconnectPolicy, BackendRegistrationValidator, BackendRegistry,
    SystemdRegistrationValidator, SYSTEM_BACKEND_RUNTIME_DIR,
};
use pronk_core::grant::GrantProvider;
use pronk_core::grant_helper::provider::PkexecAdminGrantProvider;
use pronk_dbus::BUS_NAME;
use tokio::runtime::Builder;
use tracing::{info, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};
use zbus::fdo::DBusProxy;
use zbus::names::BusName;

const SYSTEM_SERVICE_USER: &str = "pronk";
const SYSTEM_MEDIA_RUNTIME_DIRECTORY: &str = "/run/pronk";

fn main() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.as_slice() == [std::ffi::OsStr::new("--version")] {
        println!("pronk {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mode = ServiceMode::parse(&arguments)?;

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
    runtime.block_on(run(mode))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceMode {
    Session,
    System,
}

impl ServiceMode {
    fn parse(arguments: &[std::ffi::OsString]) -> anyhow::Result<Self> {
        match arguments {
            [] => Ok(Self::Session),
            [argument] if argument == "--session" => Ok(Self::Session),
            [argument] if argument == "--system" => Ok(Self::System),
            _ => anyhow::bail!("usage: pronkd [--session|--system]"),
        }
    }

    fn public_bus(self) -> PublicBus {
        match self {
            Self::Session => PublicBus::Session,
            Self::System => PublicBus::System,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::System => "system",
        }
    }
}

async fn run(mode: ServiceMode) -> anyhow::Result<()> {
    let system_connection = match mode {
        ServiceMode::Session => Some(
            zbus::Connection::system()
                .await
                .context("connect to the system bus for service arbitration")?,
        ),
        ServiceMode::System => None,
    };
    let system_bus = match &system_connection {
        Some(connection) => Some(
            DBusProxy::new(connection)
                .await
                .context("create the system bus proxy for service arbitration")?,
        ),
        None => None,
    };
    let mut system_service_appearances = match &system_bus {
        Some(bus) => Some(
            bus.receive_name_owner_changed_with_args(&[(0, BUS_NAME), (1, "")])
                .await
                .context("watch for the Pronk system service")?,
        ),
        None => None,
    };
    if let Some(bus) = &system_bus {
        let name = BusName::try_from(BUS_NAME).context("validate the Pronk bus name")?;
        if bus
            .name_has_owner(name)
            .await
            .context("query the Pronk system service")?
        {
            info!("Pronk system service is already running; session service will not start");
            return Ok(());
        }
    }

    let effective_uid = Uid::effective();
    let (runtime_directory, media_runtime) = match mode {
        ServiceMode::Session => {
            let directory = PathBuf::from(format!("/run/user/{}", effective_uid.as_raw()));
            (directory, MediaRuntime::for_user(effective_uid.as_raw()))
        }
        ServiceMode::System => {
            let service_user = User::from_name(SYSTEM_SERVICE_USER)
                .context("resolve the pronk system account")?
                .context("the pronk system account is not installed")?;
            if service_user.uid.is_root() {
                anyhow::bail!("the pronk system account must not be root");
            }
            require_system_service_identity(&service_user)?;
            drop_inheritable_process_capabilities()?;
            let media_directory = PathBuf::from(SYSTEM_MEDIA_RUNTIME_DIRECTORY);
            (
                PathBuf::from(SYSTEM_BACKEND_RUNTIME_DIR),
                MediaRuntime::new(media_directory, service_user.uid.as_raw()),
            )
        }
    };
    let registry = BackendRegistry::load_installed(&runtime_directory)
        .context("load the installed backend registry")?;
    let connection = match mode {
        ServiceMode::Session => zbus::Connection::session()
            .await
            .context("connect to the session bus")?,
        ServiceMode::System => zbus::Connection::system()
            .await
            .context("connect to the system bus")?,
    };
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
    let grant_provider: Arc<dyn GrantProvider> = match mode {
        // Mutter authorizes the sender that owns Pronk's public session-bus
        // name, so grant calls must use this same connection.
        ServiceMode::Session => Arc::new(MutterGrantProvider::new(connection.clone())),
        ServiceMode::System => Arc::new(PkexecAdminGrantProvider),
    };
    let mut manager =
        ManagerActor::spawn_with_media_runtime(configs, grant_provider, media_runtime)
            .context("start the Pronk manager")?;
    register_manager(&connection, manager.handle(), mode.public_bus())
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
    let public_bus = mode.public_bus();
    let mut lifecycle_task = tokio::spawn(async move {
        serve_lifecycle_events(
            &lifecycle_connection,
            lifecycle_manager,
            lifecycle_events,
            public_bus,
        )
        .await
    });

    connection
        .request_name(BUS_NAME)
        .await
        .context("acquire the Pronk bus name")?;
    info!(
        backends = registry.len(),
        bus = mode.label(),
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
        _ = async {
            match &mut system_service_appearances {
                Some(appearances) => {
                    if appearances.next().await.is_none() {
                        std::future::pending::<()>().await;
                    }
                }
                None => std::future::pending().await,
            }
        } => {
            info!("Pronk system service appeared; stopping the session service");
            None
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
        warn!(bus = mode.label(), %error, "failed to release the Pronk bus name");
    }
    if let Some(task) = ended_task {
        anyhow::bail!("{task} task stopped unexpectedly");
    }
    info!("Pronk stopped");
    Ok(())
}

fn require_system_service_identity(service_user: &User) -> anyhow::Result<()> {
    let user_ids = getresuid().context("read system-service user IDs")?;
    if user_ids.real != service_user.uid
        || user_ids.effective != service_user.uid
        || user_ids.saved != service_user.uid
    {
        anyhow::bail!(
            "system mode requires real/effective/saved uid {} ({}), found {}/{}/{}",
            service_user.uid.as_raw(),
            SYSTEM_SERVICE_USER,
            user_ids.real.as_raw(),
            user_ids.effective.as_raw(),
            user_ids.saved.as_raw(),
        );
    }
    let group_ids = getresgid().context("read system-service group IDs")?;
    if group_ids.real != service_user.gid
        || group_ids.effective != service_user.gid
        || group_ids.saved != service_user.gid
    {
        anyhow::bail!(
            "system mode requires real/effective/saved gid {}, found {}/{}/{}",
            service_user.gid.as_raw(),
            group_ids.real.as_raw(),
            group_ids.effective.as_raw(),
            group_ids.saved.as_raw(),
        );
    }
    Ok(())
}

#[repr(C)]
struct LinuxCapabilityHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxCapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn read_process_capabilities() -> anyhow::Result<[LinuxCapabilityData; 2]> {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    let mut header = LinuxCapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [LinuxCapabilityData::default(); 2];
    // SAFETY: `header` and the two-entry version-3 capability array are
    // writable for the duration of this synchronous syscall.
    let result = unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error()).context("read system-service capabilities");
    }
    Ok(data)
}

fn drop_inheritable_process_capabilities() -> anyhow::Result<()> {
    let capabilities = read_process_capabilities()?;
    if capabilities
        .iter()
        .any(|word| word.effective != 0 || word.permitted != 0)
    {
        anyhow::bail!("system mode must start without effective or permitted capabilities");
    }

    if capabilities.iter().any(|word| word.inheritable != 0) {
        const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
        let mut header = LinuxCapabilityHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let data = [LinuxCapabilityData::default(); 2];
        // SAFETY: `header` and the two-entry version-3 capability array remain
        // readable for the duration of this synchronous syscall.
        let result = unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) };
        if result < 0 {
            return Err(std::io::Error::last_os_error())
                .context("drop inherited system-service capabilities");
        }
    }

    if read_process_capabilities()?
        .iter()
        .any(|word| word.effective != 0 || word.permitted != 0 || word.inheritable != 0)
    {
        anyhow::bail!("system mode could not drop all process capabilities");
    }
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
