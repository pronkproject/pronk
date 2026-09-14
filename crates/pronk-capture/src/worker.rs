use std::io;
use std::sync::Arc;
use std::time::Duration;

use drm_capture::RequestId;
use tokio::sync::mpsc;
use tokio::time::{interval, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{Actor, Buffer, CaptureError, Config, Frame, Layout, Reply};

pub(crate) struct Completed {
    pub request: RequestId,
    pub outcome: Result<Duration, i32>,
}

pub(crate) trait Backend: Send + 'static {
    type Owner: Send + 'static;
    fn queue(&mut self, request: RequestId, slot: usize, buffer: &Buffer) -> io::Result<()>;
    fn dequeue(&mut self) -> io::Result<Option<Completed>>;
    fn close(&mut self) -> io::Result<()>;
    fn into_owner(self) -> Self::Owner;
}

enum Use {
    Available,
    Writing { request: RequestId, reply: Reply },
    Loaned,
}

pub(crate) fn spawn<B: Backend>(
    backend: B,
    buffers: Vec<Buffer>,
    layout: Layout,
    config: Config,
) -> Actor<B::Owner> {
    let (commands, receive) = mpsc::channel(buffers.len());
    let stop = CancellationToken::new();
    let task = tokio::spawn(run(backend, buffers, layout, config, receive, stop.clone()));
    Actor {
        commands,
        stop,
        task: Some(task),
        layout,
    }
}

async fn run<B: Backend>(
    mut backend: B,
    buffers: Vec<Buffer>,
    layout: Layout,
    config: Config,
    mut commands: mpsc::Receiver<Reply>,
    stop: CancellationToken,
) -> io::Result<B::Owner> {
    let buffers: Vec<_> = buffers.into_iter().map(Arc::new).collect();
    let mut uses: Vec<_> = buffers.iter().map(|_| Use::Available).collect();
    let (returned, mut returns) = mpsc::unbounded_channel();
    let mut next = Some(1u64);
    let mut tick = interval(config.poll_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let failure = 'active: loop {
        if stop.is_cancelled() {
            break None;
        }
        tokio::select! {
            _ = stop.cancelled() => break None,
            Some(slot) = returns.recv() => {
                if !matches!(uses.get(slot), Some(Use::Loaned)) {
                    break Some(io::Error::other("unexpected capture buffer return"));
                }
                uses[slot] = Use::Available;
            }
            _ = tick.tick() => {
                if let Err(error) = drain(&mut backend, &buffers, &mut uses, layout, &returned) {
                    break Some(error);
                }
            }
            command = commands.recv() => {
                let Some(reply) = command else { break None };
                if reply.is_closed() { continue; }
                // Returning a frame makes it eligible before the next capture
                // command, even when both channels became ready together.
                while let Ok(slot) = returns.try_recv() {
                    if !matches!(uses.get(slot), Some(Use::Loaned)) {
                        break 'active Some(io::Error::other("unexpected capture buffer return"));
                    }
                    uses[slot] = Use::Available;
                }
                let writing = uses.iter().filter(|state| matches!(state, Use::Writing { .. })).count();
                let slot = uses.iter().position(|state| matches!(state, Use::Available));
                let Some(slot) = slot.filter(|_| writing < config.capacity.get() as usize) else {
                    let _ = reply.send(Err(CaptureError::Backpressure));
                    continue;
                };
                let Some(id) = next else {
                    let _ = reply.send(Err(CaptureError::Transport(io::Error::other("capture request identifiers exhausted"))));
                    continue;
                };
                let request = RequestId::new(id).unwrap();
                match backend.queue(request, slot, &buffers[slot]) {
                    Ok(()) => {
                        next = id.checked_add(1);
                        uses[slot] = Use::Writing { request, reply };
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        let _ = reply.send(Err(CaptureError::Backpressure));
                    }
                    Err(error) => {
                        let observed = error.raw_os_error().map(io::Error::from_raw_os_error)
                            .unwrap_or_else(|| io::Error::new(error.kind(), error.to_string()));
                        let _ = reply.send(Err(CaptureError::Transport(observed)));
                        break Some(error);
                    }
                }
            }
        }
    };
    commands.close();
    // Resolve callers before draining. The buffers themselves remain owned;
    // dropping a reply is not treated as ended kernel access.
    for state in &mut uses {
        if let Use::Writing { reply, .. } = std::mem::replace(state, Use::Available) {
            let _ = reply.send(Err(CaptureError::Stopped));
        }
    }
    while let Ok(reply) = commands.try_recv() {
        let _ = reply.send(Err(CaptureError::Stopped));
    }
    let deadline = Instant::now() + config.shutdown_timeout;
    loop {
        match backend.close() {
            Ok(()) => break,
            Err(error) if error.raw_os_error() == Some(nix::libc::EBUSY) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "capture stream did not retire",
                    ));
                }
                tokio::time::sleep_until((Instant::now() + config.poll_interval).min(deadline))
                    .await;
            }
            Err(error) => return Err(error),
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(backend.into_owner()),
    }
}

fn drain<B: Backend>(
    backend: &mut B,
    buffers: &[Arc<Buffer>],
    uses: &mut [Use],
    layout: Layout,
    returned: &mpsc::UnboundedSender<usize>,
) -> io::Result<()> {
    // There are at most pool-size admitted results. A broken backend must not
    // keep the worker indefinitely inside one observation pass.
    for _ in 0..buffers.len() {
        let Some(completed) = backend.dequeue()? else {
            break;
        };
        let slot = uses.iter().position(|state| {
            matches!(state, Use::Writing { request, .. } if *request == completed.request)
        }).ok_or_else(|| io::Error::other("completion does not match an admitted capture"))?;
        let Use::Writing { reply, .. } = std::mem::replace(&mut uses[slot], Use::Available) else {
            unreachable!("matched writing slot")
        };
        let result = match completed.outcome {
            Ok(timestamp) => {
                uses[slot] = Use::Loaned;
                Ok(Frame {
                    buffer: Arc::clone(&buffers[slot]),
                    slot,
                    request: completed.request,
                    timestamp,
                    layout,
                    returned: returned.clone(),
                })
            }
            Err(error) => match error.checked_neg() {
                Some(errno) if errno > 0 => Err(CaptureError::FrameFailed {
                    request: completed.request,
                    source: io::Error::from_raw_os_error(errno),
                }),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid capture completion errno",
                    ))
                }
            },
        };
        let _ = reply.send(result);
    }
    Ok(())
}
