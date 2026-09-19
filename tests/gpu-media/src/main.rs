//! Generated images through the real source actor on an explicitly supplied graph.

mod consumer;
mod decode;
mod encoded;
mod pattern;
mod production;
mod render;
mod sandbox;
mod source;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Clone, Copy)]
struct OutputSize {
    width: u32,
    height: u32,
}

impl OutputSize {
    const HD: Self = Self {
        width: 1920,
        height: 1080,
    };

    fn parse(value: &str) -> Result<Self> {
        match value {
            "1920x1080" => Ok(Self::HD),
            "2560x1440" => Ok(Self {
                width: 2560,
                height: 1440,
            }),
            "3840x2160" => Ok(Self {
                width: 3840,
                height: 2160,
            }),
            _ => anyhow::bail!("output size must be 1920x1080, 2560x1440 or 3840x2160"),
        }
    }
}

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
        None => source::Mode::Raw,
        Some(mode) if mode == "raw" => source::Mode::Raw,
        Some(mode) if mode == "va-h264" => source::Mode::VaH264,
        Some(mode) if mode == "production-va-h264" => source::Mode::ProductionVaH264,
        _ => anyhow::bail!("profile must be raw, va-h264 or production-va-h264"),
    };
    let output_format = match args.next().as_deref() {
        None if mode == source::Mode::Raw => source::OutputFormat::Xrgb,
        None => source::OutputFormat::Argb,
        Some(value) if value == "XR24" => source::OutputFormat::Xrgb,
        Some(value) if value == "AR24" => source::OutputFormat::Argb,
        Some(value) if value == "XB24" => source::OutputFormat::Xbgr,
        Some(value) if value == "AB24" => source::OutputFormat::Abgr,
        _ => anyhow::bail!("output format must be XR24, AR24, XB24 or AB24"),
    };
    let output_size = args
        .next()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("output size is not UTF-8"))
        })
        .transpose()?
        .as_deref()
        .map(OutputSize::parse)
        .transpose()?
        .unwrap_or(OutputSize::HD);
    anyhow::ensure!(args.next().is_none(), "unexpected argument");
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16)?;
    if sandbox::verify(&node)? {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let timeout = if output_size.width > 1920 { 80 } else { 30 };
    let result = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(timeout),
            source::run(&socket, &node, modifier, mode, output_format, output_size),
        )
        .await
        .context("GPU transport test timed out")?
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}
