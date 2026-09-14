//! Opt-in generic capture qualification against an unused Rust CastKMS in a VM.

mod fixture;

use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use drm_capture::{
    create_grant, Client, Completion, Destination, DestinationId, Plane, RequestId, StreamId,
};
use fixture::Fixture;
use nix::libc;
use pronk_dmabuf::{export_dependencies, Access};

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}
fn request(value: u64) -> RequestId {
    RequestId::new(value).unwrap()
}

fn assert_error<T>(result: io::Result<T>, expected: i32) -> anyhow::Result<()> {
    match result {
        Err(error) if error.raw_os_error() == Some(expected) => Ok(()),
        Err(error) => Err(error).context(format!("expected errno {expected}")),
        Ok(_) => anyhow::bail!("unexpected success, expected errno {expected}"),
    }
}

fn poll(client: &Client, deadline: Instant) -> anyhow::Result<i16> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "capture timeout");
    let mut descriptor = libc::pollfd {
        fd: client.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: The sole initialized descriptor remains writable through poll.
    let result = unsafe {
        libc::poll(
            &mut descriptor,
            1,
            remaining.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("poll capture");
    }
    ensure!(result == 1, "capture timeout");
    ensure!(
        descriptor.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
        "invalid capture readiness"
    );
    Ok(descriptor.revents)
}

fn complete(client: &Client, stream: StreamId) -> anyhow::Result<Completion> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = client.try_dequeue(stream)? {
            return Ok(result);
        }
        let events = poll(client, deadline)?;
        if events & libc::POLLHUP != 0 {
            return client
                .try_dequeue(stream)?
                .context("revoked without a terminal result");
        }
    }
}

fn register(
    client: &Client,
    id: DestinationId,
    fixture: &Fixture,
    buffer: BorrowedFd<'_>,
) -> anyhow::Result<()> {
    let (width, height) = fixture.dimensions();
    let planes = [Plane {
        buffer,
        stride: nz(fixture.stride()),
        offset: 0,
    }];
    client.register_destination(
        id,
        &Destination {
            width: nz(width),
            height: nz(height),
            format: u32::from_le_bytes(*b"XR24"),
            modifier: 0,
            planes: &planes,
        },
    )?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        anyhow::bail!("usage: pronk-drm-capture-live-test /dev/dri/cardN (unused test VM only)");
    };
    ensure!(
        args.next().is_none(),
        "expected only an explicit test device"
    );
    let mut fixture = Fixture::open(&PathBuf::from(path))?;
    let (client, control) = create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    for fd in [client.as_fd(), control.as_fd()] {
        // SAFETY: F_GETFD reads flags on the live borrowed descriptor without arguments.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        ensure!(
            flags >= 0 && flags & libc::FD_CLOEXEC != 0,
            "grant descriptor lacks CLOEXEC"
        );
    }
    assert_error(
        Client::from_fd(control.as_fd().try_clone_to_owned()?),
        libc::ENOTTY,
    )?;
    let duplicate = Client::from_fd(client.as_fd().try_clone_to_owned()?)?;
    let description = client.describe()?;
    ensure!((description.width.get(), description.height.get()) == fixture.dimensions());
    let stream = StreamId::new(1).unwrap();
    let destination = DestinationId::new(1).unwrap();
    let source = DestinationId::new(2).unwrap();
    client.open_stream(stream, description.offer, nz(1))?;
    register(&client, destination, &fixture, fixture.destination())?;
    register(&client, source, &fixture, fixture.source())?;
    assert_error(
        client.queue_output(stream, request(1), source, None),
        libc::EINVAL,
    )?;
    ensure!(client.try_dequeue(stream)?.is_none());
    let reuse = export_dependencies(fixture.destination(), Access::Write)?;
    client.queue_output(stream, request(1), destination, Some(reuse.as_fd()))?;
    drop(reuse);
    assert_error(
        client.queue_output(stream, request(2), destination, None),
        libc::EAGAIN,
    )?;
    let result = complete(&duplicate, stream)?;
    ensure!(result.request() == request(1) && result.outcome().is_ok());
    fixture.check_pixels(0x49);
    assert_error(
        client.queue_output(stream, request(1), destination, None),
        libc::ESTALE,
    )?;
    ensure!(client.try_dequeue(stream)?.is_none());

    client.queue_output(stream, request(2), destination, None)?;
    let cancelled = match client.cancel(stream, request(2)) {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(libc::EALREADY) => false,
        Err(error) => return Err(error.into()),
    };
    let result = complete(&client, stream)?;
    ensure!(result.request() == request(2));
    if cancelled {
        ensure!(result.outcome() == Err(-libc::ECANCELED));
    } else {
        ensure!(result.outcome().is_ok());
    }

    fixture.flip();
    client.queue_output(stream, request(3), destination, None)?;
    client.unregister_destination(destination)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while poll(&client, deadline)? & libc::POLLIN == 0 {}
    drop(control);
    while poll(&client, deadline)? & libc::POLLHUP == 0 {}
    assert_error(client.describe(), libc::EKEYREVOKED)?;
    let result = complete(&client, stream)?;
    ensure!(result.request() == request(3) && result.outcome().is_ok());
    fixture.check_pixels(0x68);
    client.close_stream(stream)?;
    assert_error(client.cancel(stream, request(3)), libc::ENOENT)?;
    client.unregister_destination(source)?;
    drop(duplicate);
    drop(client);

    let (client, control) = create_grant(
        fixture.master(),
        nz(fixture.crtc()),
        nz(fixture.connector()),
    )?;
    let description = client.describe()?;
    client.open_stream(stream, description.offer, nz(1))?;
    register(&client, destination, &fixture, fixture.destination())?;
    client.queue_output(stream, request(1), destination, None)?;
    ensure!(complete(&client, stream)?.outcome().is_ok());
    fixture.check_pixels(0x68);
    drop(fixture);
    let deadline = Instant::now() + Duration::from_secs(5);
    while poll(&client, deadline)? & libc::POLLHUP == 0 {}
    assert_error(client.describe(), libc::EKEYREVOKED)?;
    client.close_stream(stream)?;
    drop(client);
    drop(control);
    println!("PASS: Rust capture client, changing pixels, backpressure, cancellation, revocation and restart");
    Ok(())
}
