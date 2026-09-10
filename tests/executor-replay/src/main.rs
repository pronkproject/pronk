//! Explicit input/output files; no daemon, native descriptors or device access.

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};

use anyhow::{bail, Context, Result};
use drm_executor_replay::{render, MAX_FIXTURE_BYTES};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (Some(input), Some(output), None) = (args.next(), args.next(), args.next()) else {
        bail!("usage: drm-executor-replay SCENE.json NEW-IMAGE.ppm");
    };
    let mut bytes = Vec::new();
    File::open(input)
        .context("open scene fixture")?
        .take(MAX_FIXTURE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read bounded scene fixture")?;
    let rendered = render(&bytes)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .context("create new output image")?;
    let mut output = BufWriter::new(file);
    rendered.write_ppm(&mut output)?;
    output.flush().context("flush output image")
}
