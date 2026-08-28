use std::fs::File;
use std::io::{self, Write};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_pipewire::{
    ClassifiedSocketPaths, ClassifiedSocketRemoteProvider, VideoBuffer, VideoBufferLayout,
    VideoSource, VideoSourceConfig, VideoSourceEvent, VideoSourceRuntimeError,
};

const START_TIMEOUT: Duration = Duration::from_secs(3);
const POLICY_LOSS_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("policy gate requires XDG_RUNTIME_DIR")?;
    let paths = ClassifiedSocketPaths::in_runtime_dir(runtime_dir)
        .context("construct classified PipeWire socket paths")?;
    let remote = ClassifiedSocketRemoteProvider::new(paths)
        .create_producer_remote()
        .await
        .context("connect classified producer remote")?
        .into_remote();

    let process_id = std::process::id();
    let mut source = VideoSource::start_with_timeout(
        VideoSourceConfig {
            node_name: format!("pronk.policy.loss-probe.{process_id}"),
            node_description: "Pronk policy-loss probe".into(),
            session_id: format!("policy-loss-probe-{process_id}"),
            device_instance: "policy-loss-probe".into(),
            connector_id: nonzero32(1),
            output_index: 0,
            grant_id: nonzero32(1),
            media_generation: nonzero64(1),
            refresh_hz: nonzero32(60),
        },
        vec![video_buffer(1)?, video_buffer(2)?],
        remote,
        START_TIMEOUT,
    )
    .await
    .context("start private PipeWire source behind policy gate")?;

    println!("ready");
    io::stdout().flush().context("flush readiness marker")?;

    let failure = tokio::time::timeout(POLICY_LOSS_TIMEOUT, async {
        loop {
            match source.next_event().await {
                Some(VideoSourceEvent::Failed(error)) => break Ok(error),
                Some(VideoSourceEvent::BufferAvailable { .. })
                | Some(VideoSourceEvent::BufferReleased { .. }) => {}
                Some(VideoSourceEvent::Stopped) => {
                    break Err(anyhow::anyhow!(
                        "private source stopped without reporting policy loss"
                    ));
                }
                None => {
                    break Err(anyhow::anyhow!(
                        "private source event channel closed without reporting policy loss"
                    ));
                }
            }
        }
    })
    .await
    .context("timed out waiting for private-policy loss")??;

    ensure!(
        failure == VideoSourceRuntimeError::PolicyUnavailable,
        "private source failed for the wrong reason: {failure}"
    );
    source
        .shutdown()
        .await
        .context("join private PipeWire source after policy loss")?;
    println!("policy_loss=pass");
    Ok(())
}

fn video_buffer(id: u32) -> anyhow::Result<VideoBuffer> {
    Ok(VideoBuffer {
        id: nonzero32(id),
        // No consumer is attached in this gate, so PipeWire never imports the
        // placeholder descriptors. Production descriptors are DMA-BUFs owned
        // by the capture generation; this probe exercises only policy life.
        dma_buf: File::open("/dev/null")
            .context("open placeholder video descriptor")?
            .into(),
        layout: VideoBufferLayout {
            width: nonzero32(320),
            height: nonzero32(180),
            pitch: nonzero32(1_280),
            size: nonzero64(230_400),
            modifier: 0,
        },
        timelines: None,
    })
}

fn nonzero32(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("test constants are nonzero")
}

fn nonzero64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("test constants are nonzero")
}
