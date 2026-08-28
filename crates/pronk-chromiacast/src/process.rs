use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use pronk_backend_protocol::{
    backend_peer_builder, require_same_uid, BackendHost1Proxy, BackendInfo, RegistrationReply,
    Validate, BACKEND_PATH,
};
use pronk_systemd::{notify_ready, notify_stopping, BackendPeerPolicy};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zbus::MessageStream;

use crate::backend::ChromiacastBackend;
use crate::device::{ChromiacastDeviceConnector, DeviceConnector, FixtureDeviceConnector};
use crate::discovery::{
    ChromiacastDiscoverySource, DiscoveryActor, DiscoveryConfiguration, DiscoverySource,
    EmptyTestDiscoverySource, FixtureTestDiscoverySource, CHROMIACAST_BACKEND_ID,
};

const TEST_MODE_ENV: &str = "PRONK_CHROMIACAST_TEST_MODE";
const SESSION_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(7);
const DISCOVERY_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
const SIGNAL_TASK_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const CONNECTION_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendMode {
    Production,
    EmptyTest,
    FixtureTest,
}

#[derive(Debug, Clone)]
pub struct StartupConfiguration {
    info: BackendInfo,
    backend_mode: BackendMode,
}

impl StartupConfiguration {
    pub fn from_environment(peer_policy: &BackendPeerPolicy) -> anyhow::Result<Self> {
        let info = BackendInfo::v1(
            CHROMIACAST_BACKEND_ID,
            "Google Cast device backend",
            env!("CARGO_PKG_VERSION"),
            environment_or("PRONK_BACKEND_INSTANCE", CHROMIACAST_BACKEND_ID),
            environment_or("INVOCATION_ID", "development"),
        );
        info.validate()
            .context("validate Chromiacast BackendInfo")?;
        let backend_mode = match std::env::var(TEST_MODE_ENV).as_deref() {
            Err(std::env::VarError::NotPresent) => BackendMode::Production,
            Ok("empty") if peer_policy.is_unmanaged_test() => BackendMode::EmptyTest,
            Ok("fixture") if peer_policy.is_unmanaged_test() => BackendMode::FixtureTest,
            Ok(value) => {
                anyhow::bail!("{TEST_MODE_ENV} value {value:?} requires unmanaged test peer policy")
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("{TEST_MODE_ENV} is not UTF-8")
            }
        };
        Ok(Self { info, backend_mode })
    }

    fn discovery_source(&self) -> Box<dyn DiscoverySource> {
        match self.backend_mode {
            BackendMode::Production => Box::new(ChromiacastDiscoverySource),
            BackendMode::EmptyTest => Box::new(EmptyTestDiscoverySource),
            BackendMode::FixtureTest => Box::new(FixtureTestDiscoverySource),
        }
    }

    fn device_connector(&self) -> Arc<dyn DeviceConnector> {
        match self.backend_mode {
            BackendMode::Production | BackendMode::EmptyTest => {
                Arc::new(ChromiacastDeviceConnector)
            }
            BackendMode::FixtureTest => Arc::new(FixtureDeviceConnector),
        }
    }
}

pub async fn run(
    stream: StdUnixStream,
    peer_policy: BackendPeerPolicy,
    configuration: StartupConfiguration,
) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::from_std(stream)
        .context("adopt activated backend control stream")?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let (discovery_actor, discovery, discovery_events) = DiscoveryActor::spawn(
        configuration.discovery_source(),
        DiscoveryConfiguration::default(),
    );
    let backend = ChromiacastBackend::new(
        configuration.info.clone(),
        discovery,
        configuration.device_connector(),
        shutdown_tx,
    );
    let connection = backend_peer_builder(stream)
        .serve_at(BACKEND_PATH, backend.clone())
        .context("export Backend1")?
        .build()
        .await
        .context("authenticate private P2P D-Bus client")?;
    require_same_uid(&connection)
        .await
        .context("authenticate Pronk peer UID")?;
    peer_policy
        .validate(&connection)
        .await
        .context("validate Pronk peer service identity")?;

    let signal_backend = backend.clone();
    let signal_connection = connection.clone();
    let signal_task = tokio::spawn(async move {
        signal_backend
            .forward_discovery_events(signal_connection, discovery_events)
            .await
    });
    let host = BackendHost1Proxy::new(&connection)
        .await
        .context("create BackendHost1 proxy")?;
    let reply: RegistrationReply = host
        .register_backend(configuration.info)
        .await
        .context("register Chromiacast backend")?;
    reply.validate().context("validate registration reply")?;
    backend.complete_registration(reply.connection_generation);

    notify_ready().context("notify systemd that Chromiacast backend is ready")?;
    let ending = wait_for_connection_end(&connection, &mut shutdown_rx).await;
    let requested_shutdown = matches!(ending, Ok(ConnectionEnd::RequestedShutdown));
    let cleanup = shutdown_runtime(
        &backend,
        discovery_actor,
        signal_task,
        connection,
        requested_shutdown,
    )
    .await;
    match (ending, cleanup) {
        (Ok(_), cleanup) => cleanup,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("backend cleanup also failed: {cleanup:#}")))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionEnd {
    RequestedShutdown,
    PeerClosed,
}

async fn wait_for_connection_end(
    connection: &zbus::Connection,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<ConnectionEnd> {
    let mut messages = MessageStream::from(connection);
    loop {
        if *shutdown.borrow() {
            return Ok(ConnectionEnd::RequestedShutdown);
        }
        tokio::select! {
            changed = shutdown.changed() => {
                changed.context("Backend1 shutdown channel closed")?;
            }
            message = messages.next() => match message {
                None | Some(Err(zbus::Error::InputOutput(_))) => {
                    return Ok(ConnectionEnd::PeerClosed);
                }
                Some(Err(error)) => {
                    return Err(error).context("read private P2P D-Bus connection");
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

async fn shutdown_runtime(
    backend: &ChromiacastBackend,
    discovery_actor: DiscoveryActor,
    signal_task: JoinHandle<zbus::Result<()>>,
    connection: zbus::Connection,
    requested_shutdown: bool,
) -> anyhow::Result<()> {
    let stopping = notify_stopping().context("notify systemd that Chromiacast backend is stopping");
    let session_cleanup = async {
        tokio::time::timeout(SESSION_SHUTDOWN_TIMEOUT, backend.shutdown_active_session())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                "active Chromiacast device session did not stop within {SESSION_SHUTDOWN_TIMEOUT:?}"
            )
            })?
            .context("stop active Chromiacast device session")
    };
    let discovery_cleanup = async {
        tokio::time::timeout(DISCOVERY_SHUTDOWN_TIMEOUT, discovery_actor.shutdown())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Chromiacast discovery actor did not stop within {DISCOVERY_SHUTDOWN_TIMEOUT:?}"
                )
            })?
            .context("stop Chromiacast discovery actor")
    };
    // These owners are independent. A wedged Device connection must not keep
    // the discovery actor alive, or vice versa.
    let (session, discovery) = tokio::join!(session_cleanup, discovery_cleanup);

    let mut signal_task = signal_task;
    let signals = match tokio::time::timeout(SIGNAL_TASK_SHUTDOWN_TIMEOUT, &mut signal_task).await {
        Ok(result) => result
            .context("join discovery signal forwarder")
            .and_then(|result| result.context("forward discovery signals")),
        Err(_) => {
            signal_task.abort();
            let _ = signal_task.await;
            Err(anyhow::anyhow!(
                "discovery signal forwarder did not stop within {SIGNAL_TASK_SHUTDOWN_TIMEOUT:?}"
            ))
        }
    };
    let close = tokio::time::timeout(CONNECTION_CLOSE_TIMEOUT, connection.close())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "private P2P D-Bus connection did not close within {CONNECTION_CLOSE_TIMEOUT:?}"
            )
        })
        .and_then(|result| result.context("close private P2P D-Bus connection"));
    let close = if requested_shutdown {
        close
    } else {
        // A peer-side close can make a local close report the already-observed
        // transport error. The resources are released either way.
        Ok(())
    };

    let mut failures = Vec::new();
    for result in [stopping, session, discovery, signals, close] {
        if let Err(error) = result {
            failures.push(format!("{error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

fn environment_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.into())
}
