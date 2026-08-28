use std::path::PathBuf;

use anyhow::{bail, Context};
use pronk_core::output::discover_castkms_outputs;
use pronk_pipewire::{
    CastKmsAudioSinkRequest, CastKmsAudioSinkResolver, ClassifiedSocketPaths,
    ClassifiedSocketRemoteProvider,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let output_index = parse_output_index()?;
    let output = discover_castkms_outputs()
        .context("discover CastKMS outputs")?
        .into_iter()
        .find(|output| output.id.output_index == output_index)
        .with_context(|| format!("CastKMS output index {output_index} is absent"))?;
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("audio sink resolver test requires XDG_RUNTIME_DIR")?;
    let provider = ClassifiedSocketRemoteProvider::new(
        ClassifiedSocketPaths::in_runtime_dir(runtime_dir)
            .context("construct classified PipeWire paths")?,
    );
    let remote = provider
        .create_producer_remote()
        .await
        .context("connect classified PipeWire audio resolver")?
        .into_remote();
    let target = CastKmsAudioSinkResolver::default()
        .resolve(
            CastKmsAudioSinkRequest {
                device_path: output.id.device_path.clone(),
                output_index,
            },
            remote,
        )
        .await
        .context("resolve connector-bound CastKMS audio sink")?;
    println!("device_path={}", output.id.device_path.display());
    println!("output_index={output_index}");
    println!("node_name={}", target.node_name);
    println!("object_id={}", target.object_id);
    println!("object_serial={}", target.object_serial);
    println!("connector_bound_audio_sink=pass");
    Ok(())
}

fn parse_output_index() -> anyhow::Result<u32> {
    let mut arguments = std::env::args();
    let program = arguments.next().unwrap_or_default();
    let Some(output_index) = arguments.next() else {
        bail!("usage: {program} OUTPUT-INDEX");
    };
    if arguments.next().is_some() {
        bail!("usage: {program} OUTPUT-INDEX");
    }
    output_index.parse().context("parse CastKMS output index")
}
