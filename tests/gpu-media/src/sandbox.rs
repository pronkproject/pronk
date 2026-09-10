//! Assertions about the effective transient fixture sandbox, not installed policy.

use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{ensure, Context, Result};

/// Returns true only for the completed denied-device control experiment.
pub fn verify(render_node: &Path) -> Result<bool> {
    let Some(mode) = std::env::var_os("PRONK_GPU_TEST_SANDBOX") else {
        return Ok(false);
    };
    ensure!(
        mode == "allowed" || mode == "denied",
        "invalid sandbox mode"
    );
    let status = std::fs::read_to_string("/proc/self/status")?;
    for (name, expected) in [
        ("NoNewPrivs:", "1"),
        ("Seccomp:", "2"),
        ("CapEff:", "0000000000000000"),
    ] {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .context("process security status")?;
        ensure!(value.trim() == expected, "sandbox {name} is not {expected}");
    }
    // SAFETY: PR_GET_MDWE reads the calling process's flags, with no pointers.
    let mdwe = unsafe {
        libc::prctl(
            libc::PR_GET_MDWE,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    ensure!(
        mdwe >= 0 && (mdwe as libc::c_uint) & libc::PR_MDWE_REFUSE_EXEC_GAIN != 0,
        "sandbox does not prohibit executable permission gain"
    );
    network_denied(std::net::TcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        0,
    )))?;
    network_denied(std::net::TcpListener::bind((
        std::net::Ipv6Addr::LOCALHOST,
        0,
    )))?;
    let denied = mode == "denied";
    match OpenOptions::new().read(true).write(true).open(render_node) {
        Ok(_) => ensure!(!denied, "denied sandbox exposes the selected GPU"),
        Err(error) => {
            ensure!(
                denied
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ),
                "selected GPU access failed unexpectedly: {error}"
            );
        }
    }
    match std::fs::read_dir("/dev/dri") {
        Ok(entries) => {
            for entry in entries {
                ensure!(
                    !denied && entry?.path() == render_node,
                    "sandbox exposes an unrelated DRM node"
                );
            }
        }
        Err(error) => {
            ensure!(
                denied && error.kind() == std::io::ErrorKind::NotFound,
                "cannot inspect sandbox DRM namespace: {error}"
            );
        }
    }
    eprintln!(
        "PASS: effective sandbox restrictions, selected GPU access={}",
        if denied { "denied" } else { "allowed" }
    );
    Ok(denied)
}

fn network_denied(result: std::io::Result<std::net::TcpListener>) -> Result<()> {
    match result {
        Ok(_) => anyhow::bail!("sandbox permits network sockets"),
        Err(error) => ensure!(
            matches!(
                error.raw_os_error(),
                Some(libc::EAFNOSUPPORT | libc::EPERM | libc::EACCES)
            ),
            "network failure does not establish sandbox denial: {error}"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_network_failures_do_not_qualify_isolation() {
        for error in [libc::EAFNOSUPPORT, libc::EPERM, libc::EACCES] {
            assert!(network_denied(Err(std::io::Error::from_raw_os_error(error))).is_ok());
        }
        for error in [libc::EADDRINUSE, libc::EADDRNOTAVAIL, libc::EMFILE] {
            assert!(network_denied(Err(std::io::Error::from_raw_os_error(error))).is_err());
        }
    }
}
