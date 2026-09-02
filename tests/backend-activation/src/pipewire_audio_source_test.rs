use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use nix::fcntl::OFlag;
use nix::unistd::{pipe2, Uid};
use pronk_pipewire::{
    AudioSource, AudioSourceConfig, ClassifiedSocketPaths, ClassifiedSocketRemoteProvider,
};

const PERIOD_FRAMES: usize = 480;
const FRAME_BYTES: usize = 4;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .context("audio source test requires XDG_RUNTIME_DIR")?;
    let paths = ClassifiedSocketPaths::in_runtime_dir(runtime_dir)?;
    let provider = ClassifiedSocketRemoteProvider::new_for_server_uid(paths, Uid::effective());
    let remote = provider.create_producer_remote().await?;
    let (tap, writer) = synthetic_tap()?;
    let running = Arc::new(AtomicBool::new(true));
    let writer_running = Arc::clone(&running);
    let writer_task = std::thread::Builder::new()
        .name("pronk-test-audio-tap".into())
        .spawn(move || write_tap(writer, &writer_running))?;

    let source = AudioSource::start(
        AudioSourceConfig {
            node_name: "pronk.policy.test.kernel-audio".into(),
            node_description: "Pronk policy kernel audio".into(),
            session_id: "policy-audio-test".into(),
            device_instance: "policy-device".into(),
            connector_id: NonZeroU32::new(42).unwrap(),
            output_index: 1,
            grant_id: NonZeroU32::new(7).unwrap(),
            media_generation: NonZeroU64::new(1).unwrap(),
        },
        tap,
        remote.into_remote(),
    )
    .await?;
    println!(
        "ready name={} serial={}",
        source.identity().node_name,
        source.identity().object_serial
    );

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    source.shutdown().await?;
    running.store(false, Ordering::Release);
    writer_task
        .join()
        .map_err(|_| anyhow::anyhow!("synthetic audio writer panicked"))?;
    Ok(())
}

fn synthetic_tap() -> anyhow::Result<(OwnedFd, OwnedFd)> {
    pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).context("create synthetic audio tap")
}

fn write_tap(writer: OwnedFd, running: &AtomicBool) {
    let mut period = [0_u8; PERIOD_FRAMES * FRAME_BYTES];
    for (frame, samples) in period.chunks_exact_mut(FRAME_BYTES).enumerate() {
        let sample = (((frame as i32 * 97) % 20_000) - 10_000) as i16;
        samples[..2].copy_from_slice(&sample.to_le_bytes());
        samples[2..].copy_from_slice(&sample.to_le_bytes());
    }
    while running.load(Ordering::Acquire) {
        match nix::unistd::write(&writer, &period) {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
