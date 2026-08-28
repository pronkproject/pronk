use std::time::Duration;

use anyhow::Context;
use pronk_systemd::take_backend_control_fd;
use tokio::io::AsyncWriteExt;
use tokio::runtime::Builder;

const ACKNOWLEDGEMENT: &[u8] = b"PRNK-ACTIVATION-V1\n";

fn main() -> anyhow::Result<()> {
    // LISTEN_* is consumed while startup is still single-threaded. Constructing
    // Tokio first would make sd-notify's environment-unset operation unsound.
    let control = take_backend_control_fd().context("take backend control fd")?;
    let stream = control.into_std_stream();

    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("create Tokio runtime")?;
    runtime.block_on(async move {
        let mut stream =
            tokio::net::UnixStream::from_std(stream).context("adopt activated Unix stream")?;
        stream
            .write_all(ACKNOWLEDGEMENT)
            .await
            .context("write activation acknowledgement")?;
        stream.shutdown().await.context("close activated stream")?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(())
    })
}
