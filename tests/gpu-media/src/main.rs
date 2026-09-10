//! Generated images through the real source actor on an explicitly supplied graph.

mod consumer;
mod decode;
mod encoded;
mod pattern;
mod sandbox;
mod source;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let socket = PathBuf::from(
        args.next()
            .context("usage: pronk-gpu-media-test SOCKET RENDER_NODE HEX_MODIFIER")?,
    );
    let node = PathBuf::from(args.next().context("missing render node")?);
    let modifier = args
        .next()
        .context("missing modifier")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("modifier is not UTF-8"))?;
    let mode = match args.next().as_deref() {
        None => consumer::Mode::Raw,
        Some(mode) if mode == "raw" => consumer::Mode::Raw,
        Some(mode) if mode == "va-h264" => consumer::Mode::VaH264,
        _ => anyhow::bail!("profile must be raw or va-h264"),
    };
    anyhow::ensure!(args.next().is_none(), "unexpected argument");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16)?;
    if sandbox::verify(&node)? {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(30),
            source::run(&socket, &node, modifier, mode),
        )
        .await
        .context("GPU transport test timed out")?
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}
