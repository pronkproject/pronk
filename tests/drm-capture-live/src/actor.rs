//! Opt-in capture actor qualification. Never run on an occupied host display.

#[allow(dead_code)] // The ordinary KMS fixture is shared with the low-level probe.
mod fixture;

use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use drm_capture::{create_grant, Client, DestinationId, StreamId};
use pronk_capture::{
    allocation::Heap, Actor, Buffer, CaptureError, Config, Frame, Layout, Session,
};

extern "C" {
    fn capture_buffer_check_pixels(dma: i32, width: u32, height: u32, stride: u32, expected: u8);
}

fn check(frame: &Frame, expected: u8) {
    let layout = frame.layout();
    // SAFETY: The completed frame retains its DMA-BUF throughout the read-only
    // check. The test helper maps the described extent and brackets CPU access.
    unsafe {
        capture_buffer_check_pixels(
            frame.as_fd().as_raw_fd(),
            layout.width.get(),
            layout.height.get(),
            frame.stride().get(),
            expected,
        );
    }
}

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

async fn frame<F: AsFd + Send + 'static>(actor: &Actor<F>) -> anyhow::Result<Frame> {
    tokio::time::timeout(Duration::from_secs(5), actor.capture())
        .await
        .context("capture actor timeout")?
        .context("capture frame")
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(
        args.next()
            .context("usage: pronk-capture-actor-live-test /dev/dri/cardN (unused VM only)")?,
    );
    ensure!(args.next().is_none(), "one test device is required");
    let mut fixture = fixture::Fixture::open(&path)?;
    let heap = Heap::open(Path::new("/dev/dma_heap/system"))?;
    let layout = Layout {
        width: nz(640),
        height: nz(480),
    };
    let config = Config {
        capacity: nz(3),
        poll_interval: Duration::from_millis(2),
        shutdown_timeout: Duration::from_secs(5),
    };
    let budget = NonZeroU64::new(16 * 1024 * 1024).unwrap();
    let (client, control) = create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let observer = Client::from_fd(client.as_fd().try_clone_to_owned()?)?;
            let mut session = Session::new(client);
            let mut incomplete = heap.allocate(layout, nz(3), budget)?;
            incomplete[1] = Buffer::new(std::fs::File::open("/dev/null")?.into(), nz(640 * 4));
            ensure!(
                session.spawn(incomplete, config).is_err(),
                "an ordinary file was accepted as a DMA-BUF destination"
            );
            ensure!(
                observer
                    .close_stream(StreamId::new(1).unwrap())
                    .unwrap_err()
                    .raw_os_error()
                    == Some(nix::libc::ENOENT),
                "failed setup retained its stream"
            );
            ensure!(
                observer
                    .unregister_destination(DestinationId::new(1).unwrap())
                    .unwrap_err()
                    .raw_os_error()
                    == Some(nix::libc::ENOENT),
                "failed setup retained its first destination"
            );
            drop(observer);
            let actor = session.spawn(heap.allocate(layout, nz(3), budget)?, config)?;
            let first = frame(&actor).await?;
            let second = frame(&actor).await?;
            let third = frame(&actor).await?;
            for image in [&first, &second, &third] {
                check(image, 0x49);
            }
            ensure!(
                matches!(actor.capture().await, Err(CaptureError::Backpressure)),
                "held pool was reused"
            );

            // Downstream-held images must not prevent an ordinary framebuffer flip.
            fixture.flip();
            check(&first, 0x49);
            drop(second);
            drop(third);
            let changed = frame(&actor).await?;
            check(&changed, 0x68);
            drop(changed);
            drop(actor.shutdown().await?);

            // Each media generation keeps the grant but owns fresh pool storage.
            for _ in 0..3 {
                let actor = session.spawn(heap.allocate(layout, nz(3), budget)?, config)?;
                let current = frame(&actor).await?;
                check(&current, 0x68);
                check(&first, 0x49);
                drop(current);
                drop(actor.shutdown().await?);
            }
            drop(session);
            drop(control);

            // A fresh authorization uses new backing storage, not the retained image.
            let (client, control) = create_grant(
                fixture.master(),
                nz(fixture.crtc()),
                nz(fixture.connector()),
            )?;
            let actor =
                Session::new(client).spawn(heap.allocate(layout, nz(3), budget)?, config)?;
            let restarted = frame(&actor).await?;
            check(&restarted, 0x68);
            check(&first, 0x49);
            drop(restarted);
            drop(first);
            drop(actor.shutdown().await?);
            drop(control);
            Ok::<(), anyhow::Error>(())
        })?;
    println!("PASS: capture actor, setup rollback, held frames, changing pixels and restart");
    Ok(())
}
