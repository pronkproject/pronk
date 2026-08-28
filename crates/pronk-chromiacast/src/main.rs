use anyhow::Context;
use pronk_chromiacast::{run, StartupConfiguration};
use pronk_systemd::{take_backend_control_fd, BackendPeerPolicy};
use tokio::runtime::Builder;
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

fn main() -> anyhow::Result<()> {
    // Consume and unset LISTEN_* synchronously, before Tokio or any library
    // worker thread exists. Ambient PipeWire selection is never part of the
    // backend contract and must not cross startup either.
    let control = take_backend_control_fd().context("take backend control fd")?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize logging: {error}"))?;
    scrub_ambient_pipewire_environment();
    let peer_policy =
        BackendPeerPolicy::from_environment().context("load root-owned backend peer policy")?;
    let configuration = StartupConfiguration::from_environment(&peer_policy)?;
    let stream = control.into_std_stream();

    Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?
        .block_on(run(stream, peer_policy, configuration))
}

fn scrub_ambient_pipewire_environment() {
    for name in ["PIPEWIRE_REMOTE", "PIPEWIRE_RUNTIME_DIR"] {
        std::env::remove_var(name);
    }
}
