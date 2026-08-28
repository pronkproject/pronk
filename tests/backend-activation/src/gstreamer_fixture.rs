use std::num::NonZeroU64;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use serde_json::Value;

const START_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn pass_gstreamer_diagnostic_environment(command: &mut Command) {
    // Socket activation needs opt-in diagnostics in the backend's explicit
    // environment, just as a real systemd service would.
    for variable in ["GST_DEBUG", "GST_DEBUG_NO_COLOR", "GST_DEBUG_FILE"] {
        if std::env::var_os(variable).is_some() {
            command.arg(format!("--setenv={variable}"));
        }
    }
}

#[derive(Debug)]
pub(crate) struct GStreamerTestProducer {
    _child: KillOnDrop,
    pub(crate) node_name: String,
    pub(crate) object_serial: NonZeroU64,
}

impl GStreamerTestProducer {
    pub(crate) async fn start_with_format(
        label: &str,
        width: u32,
        height: u32,
        frames_per_second: u32,
    ) -> anyhow::Result<Self> {
        let gst_launch = std::env::var_os("PRONK_MEDIA_GATE_GST_LAUNCH")
            .context("media gate requires PRONK_MEDIA_GATE_GST_LAUNCH")?;
        let node_name = format!("pronk.media.gate.{}.{label}", std::process::id());
        let caps = format!(
            "video/x-raw,format=BGRx,width={width},height={height},framerate={frames_per_second}/1"
        );
        let child = Command::new(&gst_launch)
            .arg("-q")
            .arg("videotestsrc")
            .arg("is-live=true")
            .arg("pattern=smpte")
            .arg("!")
            .arg(caps)
            .arg("!")
            .arg("pipewiresink")
            .arg("mode=provide")
            .arg("client-name=pronk-media-gate-producer")
            .arg(format!(
                "stream-properties=props,node.name={node_name},node.description=PronkMediaGate,media.class=Video/Source,media.role=Screen,api.pronk.private=v1"
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start GStreamer PipeWire test producer")?;
        let mut child = KillOnDrop(child);

        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(status) = child
                .0
                .try_wait()
                .context("query GStreamer test producer")?
            {
                bail!("GStreamer test producer exited early with {status}");
            }
            if let Some(object_serial) = find_pipewire_node_serial(&node_name)? {
                wait_for_backend_node(&node_name, object_serial).await?;
                // WirePlumber creates its linkable session item shortly after
                // the PipeWire registry global appears. There is no public
                // non-consuming readiness query, so give that asynchronous
                // policy step one bounded fixture-only scheduling interval.
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(Self {
                    _child: child,
                    node_name,
                    object_serial,
                });
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for PipeWire node {node_name:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[derive(Debug)]
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

pub(crate) async fn wait_for_backend_pipewire_clients(
    expected_count: usize,
) -> anyhow::Result<Vec<NonZeroU64>> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let clients = backend_pipewire_clients()?;
        if clients.len() == expected_count {
            return Ok(clients);
        }
        ensure!(
            Instant::now() < deadline,
            "expected {expected_count} backend PipeWire clients; found {clients:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn find_pipewire_node_serial(node_name: &str) -> anyhow::Result<Option<NonZeroU64>> {
    find_pipewire_node_serial_on("pipewire-0-manager", node_name)
}

async fn wait_for_backend_node(node_name: &str, expected_serial: NonZeroU64) -> anyhow::Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if find_pipewire_node_serial_on("pipewire-0-pronk-backend", node_name)?
            == Some(expected_serial)
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "backend media role cannot see PipeWire node {node_name:?} ({expected_serial})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn find_pipewire_node_serial_on(
    remote: &str,
    node_name: &str,
) -> anyhow::Result<Option<NonZeroU64>> {
    Ok(pipewire_registry(remote)?.into_iter().find_map(|object| {
        let props = object.get("info")?.get("props")?;
        (object.get("type")?.as_str()? == "PipeWire:Interface:Node"
            && props.get("node.name")?.as_str()? == node_name)
            .then(|| props.get("object.serial")?.as_u64())
            .flatten()
            .and_then(NonZeroU64::new)
    }))
}

fn backend_pipewire_clients() -> anyhow::Result<Vec<NonZeroU64>> {
    let mut clients: Vec<_> = pipewire_registry("pipewire-0-manager")?
        .into_iter()
        .filter_map(|object| {
            let props = object.get("info")?.get("props")?;
            (object.get("type")?.as_str()? == "PipeWire:Interface:Client"
                && props.get("pipewire.access")?.as_str()? == "pronk-backend")
                .then(|| props.get("object.serial")?.as_u64())
                .flatten()
                .and_then(NonZeroU64::new)
        })
        .collect();
    clients.sort_unstable();
    Ok(clients)
}

fn pipewire_registry(remote: &str) -> anyhow::Result<Vec<Value>> {
    let pw_dump = std::env::var_os("PRONK_MEDIA_GATE_PW_DUMP")
        .context("media gate requires PRONK_MEDIA_GATE_PW_DUMP")?;
    let output = Command::new(pw_dump)
        .args(["-r", remote])
        .output()
        .context("query isolated PipeWire registry")?;
    ensure!(
        output.status.success(),
        "pw-dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("parse pw-dump registry JSON")
}
