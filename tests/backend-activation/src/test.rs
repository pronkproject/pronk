use std::fs;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use pronk_systemd::BACKEND_CONTROL_FD_NAME;

const ACKNOWLEDGEMENT: &[u8] = b"PRNK-ACTIVATION-V1\n";
const START_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    let (socket_activate, probe) = parse_arguments()?;
    let socket_path = temporary_socket_path();
    remove_stale_socket(&socket_path)?;

    let child = Command::new(socket_activate)
        .arg(format!("--listen={}", socket_path.display()))
        .arg("--accept")
        .arg(format!("--fdname={BACKEND_CONTROL_FD_NAME}"))
        .arg(probe)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start systemd-socket-activate")?;
    let mut launcher = LauncherGuard(Some(child));

    wait_for_socket(&socket_path, &mut launcher)?;
    let mut connection = UnixStream::connect(&socket_path)
        .with_context(|| format!("connect to {}", socket_path.display()))?;
    connection
        .set_read_timeout(Some(IO_TIMEOUT))
        .context("set activation read timeout")?;

    let mut acknowledgement = vec![0_u8; ACKNOWLEDGEMENT.len()];
    connection
        .read_exact(&mut acknowledgement)
        .context("read activation acknowledgement")?;
    ensure!(
        acknowledgement == ACKNOWLEDGEMENT,
        "activation acknowledgement differs"
    );

    let mut trailing = [0_u8; 1];
    ensure!(
        connection
            .read(&mut trailing)
            .context("wait for probe EOF")?
            == 0,
        "activation probe sent trailing data"
    );

    launcher.stop()?;
    remove_stale_socket(&socket_path)?;
    println!("systemd_named_listen_fd=pass");
    println!("systemd_connected_stream=pass");
    println!("systemd_pre_tokio_intake=pass");
    Ok(())
}

#[derive(Debug)]
struct LauncherGuard(Option<Child>);

impl LauncherGuard {
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("launcher has not been stopped")
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        let mut child = self.0.take().expect("launcher has not been stopped");
        if child
            .try_wait()
            .context("query activation launcher")?
            .is_none()
        {
            child.kill().context("stop activation launcher")?;
        }
        child.wait().context("reap activation launcher")?;
        Ok(())
    }
}

impl Drop for LauncherGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for_socket(path: &Path, launcher: &mut LauncherGuard) -> anyhow::Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if path.exists() {
            return Ok(());
        }
        if let Some(status) = launcher
            .child_mut()
            .try_wait()
            .context("query activation launcher")?
        {
            bail!("systemd-socket-activate exited early with {status}");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {}", path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn temporary_socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("pronk-activation-{}.sock", std::process::id()))
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn parse_arguments() -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let socket_activate = arguments
        .next()
        .context("missing systemd-socket-activate path")?;
    let probe = arguments.next().context("missing activation probe path")?;
    ensure!(arguments.next().is_none(), "unexpected extra argument");
    Ok((socket_activate, probe))
}
