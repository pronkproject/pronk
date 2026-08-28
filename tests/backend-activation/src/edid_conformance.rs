use std::ffi::OsStr;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{ensure, Context};

const HDMI_VSDB_EDID_14_FAILURE: &str = "Vendor-Specific Data Block (HDMI), OUI 00-0C-03: The HDMI Specification requires EDID 1.3 instead of 1.4.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceOutcome {
    Pass,
    HdmiVsdbEdid14Exception,
}

pub fn check(
    decoder: &OsStr,
    edid: &[u8],
    expected_product_name: &str,
) -> anyhow::Result<ConformanceOutcome> {
    let mut child = Command::new(decoder)
        .args(["--check", "--skip-hex-dump", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start edid-decode")?;
    child
        .stdin
        .take()
        .context("edid-decode stdin is missing")?
        .write_all(edid)
        .context("write generated EDID to edid-decode")?;
    let output = child.wait_with_output().context("wait for edid-decode")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        stdout.contains(&format!("Product ID: {expected_product_name}")),
        "edid-decode did not recover the DisplayID product name:\n{stdout}"
    );
    let failures = report_lines(&stdout, "Failures:", "EDID conformity:");
    let outcome = if failures.is_empty() {
        ensure!(
            output.status.success() && stdout.contains("EDID conformity: PASS"),
            "edid-decode rejected the generated EDID\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        ConformanceOutcome::Pass
    } else {
        ensure!(
            !output.status.success()
                && stdout.contains("EDID conformity: FAIL")
                && failures == [HDMI_VSDB_EDID_14_FAILURE],
            "edid-decode reported an unrecognized conformance failure\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        ConformanceOutcome::HdmiVsdbEdid14Exception
    };
    let warnings = stdout
        .split_once("Warnings:")
        .map(|(_, tail)| tail)
        .unwrap_or_default()
        .split("Failures:")
        .next()
        .unwrap_or_default()
        .split("EDID conformity:")
        .next()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Block "))
        .collect::<Vec<_>>();
    ensure!(
        warnings == ["Missing Display Product Name."],
        "edid-decode reported unexpected warnings:\n{stdout}"
    );
    Ok(outcome)
}

fn report_lines<'a>(report: &'a str, heading: &str, ending: &str) -> Vec<&'a str> {
    report
        .split_once(heading)
        .map(|(_, tail)| tail)
        .unwrap_or_default()
        .split(ending)
        .next()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Block "))
        .collect()
}
