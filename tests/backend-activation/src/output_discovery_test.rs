use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context};
use pronk_core::output::{discover_castkms_outputs, CastKmsOutputId, OutputConnection};

fn main() -> anyhow::Result<()> {
    let expected = parse_expected_count()?;
    let descriptors_before = descriptor_count()?;
    let first = discover_castkms_outputs().context("first CastKMS output discovery")?;
    let descriptors_after_first = descriptor_count()?;
    let second = discover_castkms_outputs().context("second CastKMS output discovery")?;
    let descriptors_after_second = descriptor_count()?;

    if first != second {
        bail!("two consecutive CastKMS output inventories differ");
    }
    if first.len() != expected {
        bail!(
            "discovered {} CastKMS outputs; expected {expected}",
            first.len()
        );
    }
    if descriptors_before != descriptors_after_first
        || descriptors_before != descriptors_after_second
    {
        bail!(
            "output discovery leaked descriptors: before={descriptors_before}, first={descriptors_after_first}, second={descriptors_after_second}"
        );
    }

    let mut identities = HashSet::with_capacity(first.len());
    for (expected_index, output) in first.iter().enumerate() {
        if output.id.output_index != expected_index as u32 {
            bail!(
                "output {} has stable index {}; expected {expected_index}",
                output.connector_id,
                output.id.output_index
            );
        }
        if output.connection != OutputConnection::Disconnected || !output.is_available() {
            bail!(
                "output {} is {:?}; a fresh test device must be disconnected",
                output.connector_id,
                output.connection
            );
        }
        if !identities.insert(CastKmsOutputId {
            device_path: output.id.device_path.clone(),
            output_index: output.id.output_index,
        }) {
            bail!("duplicate output identity {:?}", output.id);
        }
        if output.device_major == 0 || output.connector_id == 0 {
            bail!("output has an invalid device or connector identity");
        }
        println!(
            "output={} connector={} name={} device={}:{} status=disconnected",
            output.id.output_index,
            output.connector_id,
            output.connector_name,
            output.device_major,
            output.device_minor
        );
    }
    println!("stable_output_identity=pass");
    println!("output_discovery_no_fd_retention=pass");
    println!("disconnected_output_inventory=pass");
    Ok(())
}

fn parse_expected_count() -> anyhow::Result<usize> {
    let mut arguments = std::env::args_os();
    let program = arguments.next().unwrap_or_default();
    let Some(expected) = arguments.next() else {
        bail!(
            "usage: {} EXPECTED-OUTPUT-COUNT",
            Path::new(&program).display()
        );
    };
    if arguments.next().is_some() {
        bail!(
            "usage: {} EXPECTED-OUTPUT-COUNT",
            Path::new(&program).display()
        );
    }
    expected
        .to_str()
        .context("expected output count is not UTF-8")?
        .parse()
        .context("expected output count is not a nonnegative integer")
}

fn descriptor_count() -> anyhow::Result<usize> {
    std::fs::read_dir("/proc/self/fd")
        .context("read /proc/self/fd")?
        .try_fold(0_usize, |count, entry| {
            entry.context("read descriptor entry").map(|_| count + 1)
        })
}
