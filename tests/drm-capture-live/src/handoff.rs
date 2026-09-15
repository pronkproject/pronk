//! Real capture frames with synthetic transport events. No PipeWire server.

#[allow(dead_code)]
mod fixture;

use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context};
use pronk_capture::{allocation::Heap, Actor, CaptureError, Config, Frame, Layout};
use pronk_capture_pipewire::Registration;
use pronk_pipewire::{PipeWireBufferTransport, VideoNodeIdentity, VideoSourceActorEvent};

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}
fn generation(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

async fn capture(actor: &Actor<std::os::fd::OwnedFd>) -> anyhow::Result<Frame> {
    Ok(tokio::time::timeout(Duration::from_secs(5), actor.capture()).await??)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(
        args.next()
            .context("usage: pronk-capture-handoff-live-test /dev/dri/cardN (unused VM only)")?,
    );
    ensure!(args.next().is_none(), "one device is required");
    let fixture = fixture::Fixture::open(&path)?;
    let heap = Heap::open(Path::new("/dev/dma_heap/system"))?;
    let (client, control) = drm_capture::create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let actor = Actor::spawn(
        client,
        heap.allocate(
            Layout {
                width: nz(640),
                height: nz(480),
            },
            nz(2),
            generation(16 * 1024 * 1024),
        )?,
        Config {
            capacity: nz(2),
            poll_interval: Duration::from_millis(2),
            shutdown_timeout: Duration::from_secs(5),
        },
    )?;
    let registration = Registration::new(&actor)?;
    let exports = registration.export()?;
    ensure!(exports.len() == 2, "export count");
    let mut output = registration.bind(VideoNodeIdentity {
        node_name: "synthetic-transport".into(),
        object_id: nz(1),
        object_serial: generation(1),
        media_generation: generation(2),
    });
    for buffer in &exports {
        output.handle_event(&VideoSourceActorEvent::BufferAvailable {
            media_generation: generation(2),
            buffer_id: buffer.id,
            transport: PipeWireBufferTransport::ReadyBeforePublish,
        })?;
    }
    let first = output.begin_publish(capture(&actor).await?, 0, true)?;
    let second = output.begin_publish(capture(&actor).await?, 1, false)?;
    ensure!(
        first.buffer_id != second.buffer_id && first.sequence < second.sequence,
        "distinct publications"
    );
    output.handle_event(&VideoSourceActorEvent::BufferReleased {
        media_generation: generation(1),
        buffer_id: first.buffer_id,
        sequence: first.sequence,
    })?;
    ensure!(
        output
            .handle_event(&VideoSourceActorEvent::BufferReleased {
                media_generation: generation(2),
                buffer_id: first.buffer_id,
                sequence: second.sequence,
            })
            .is_err(),
        "mismatched release accepted"
    );
    ensure!(
        matches!(actor.capture().await, Err(CaptureError::Backpressure)),
        "unmatched release returned credit"
    );
    output.handle_event(&VideoSourceActorEvent::BufferReleased {
        media_generation: generation(2),
        buffer_id: first.buffer_id,
        sequence: first.sequence,
    })?;
    let third = output.begin_publish(capture(&actor).await?, 2, false)?;
    ensure!(
        third.buffer_id == first.buffer_id && third.sequence > second.sequence,
        "reuse identity"
    );
    ensure!(
        output
            .handle_event(&VideoSourceActorEvent::BufferReleased {
                media_generation: generation(2),
                buffer_id: first.buffer_id,
                sequence: first.sequence,
            })
            .is_err(),
        "old release accepted for new use"
    );
    drop(output);
    ensure!(
        matches!(actor.capture().await, Err(CaptureError::Backpressure)),
        "teardown returned uncertain credits"
    );
    drop(actor.shutdown().await?);
    drop(exports);
    drop(control);
    println!("PASS: real capture frames, synthetic transport release correlation and retirement");
    Ok(())
}
