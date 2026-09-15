use super::*;
use std::collections::VecDeque;
use std::fs::File;
use std::num::NonZeroU32;
use std::os::fd::AsFd;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    queued: Vec<(RequestId, usize)>,
    completed: VecDeque<Completed>,
    busy: bool,
    reject_once: bool,
    disconnected: bool,
    closes: usize,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    queued: Notify,
    closing: Notify,
}

struct Fake {
    shared: Arc<Shared>,
    owner: File,
}

impl Backend for Fake {
    type Owner = File;

    fn queue(&mut self, request: RequestId, slot: usize, _: &Buffer) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if std::mem::take(&mut state.reject_once) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        state.queued.push((request, slot));
        self.shared.queued.notify_one();
        Ok(())
    }

    fn dequeue(&mut self) -> io::Result<Option<Completed>> {
        let mut state = self.shared.state.lock().unwrap();
        if state.disconnected {
            return Err(io::Error::from_raw_os_error(nix::libc::EKEYREVOKED));
        }
        Ok(state.completed.pop_front())
    }

    fn close(&mut self) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        state.closes += 1;
        self.shared.closing.notify_one();
        if state.busy {
            Err(io::Error::from_raw_os_error(nix::libc::EBUSY))
        } else {
            Ok(())
        }
    }

    fn into_owner(self) -> File {
        self.owner
    }
}

fn fixture(count: usize) -> (Actor<File>, Arc<Shared>) {
    let shared = Arc::new(Shared::default());
    let backend = Fake {
        shared: shared.clone(),
        owner: File::open("/dev/null").unwrap(),
    };
    let buffers = (0..count)
        .map(|_| Buffer::new(File::open("/dev/null").unwrap().into(), nz(4)))
        .collect();
    let actor = spawn(
        backend,
        buffers,
        Layout {
            width: nz(1),
            height: nz(1),
        },
        Config {
            capacity: nz(count as u32),
            poll_interval: Duration::from_millis(1),
            shutdown_timeout: Duration::from_millis(40),
        },
    );
    (actor, shared)
}

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

#[tokio::test]
async fn idle_failure_notifies_closure_before_retirement() {
    let (actor, shared) = fixture(1);
    {
        let mut state = shared.state.lock().unwrap();
        state.disconnected = true;
        state.busy = true;
    }
    tokio::time::timeout(Duration::from_secs(2), actor.closed())
        .await
        .unwrap();
    assert!(shared.state.lock().unwrap().queued.is_empty());
    assert!(matches!(actor.capture().await, Err(CaptureError::Stopped)));
    shared.state.lock().unwrap().busy = false;
    assert_eq!(
        actor.shutdown().await.unwrap_err().raw_os_error(),
        Some(nix::libc::EKEYREVOKED)
    );
}

async fn notified(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(2), notify.notified())
        .await
        .unwrap();
}

async fn complete(shared: &Shared, outcome: Result<Duration, i32>) {
    notified(&shared.queued).await;
    let mut state = shared.state.lock().unwrap();
    let request = state.queued.last().unwrap().0;
    state.completed.push_back(Completed { request, outcome });
}

async fn frame(actor: &Actor<File>, shared: &Shared) -> Frame {
    let (result, ()) = tokio::join!(
        actor.capture(),
        complete(shared, Ok(Duration::from_nanos(42)))
    );
    result.unwrap()
}

#[tokio::test]
async fn a_held_frame_prevents_another_write_to_its_slot() {
    let (actor, shared) = fixture(1);
    let first = frame(&actor, &shared).await;
    assert_eq!(first.timestamp(), Duration::from_nanos(42));
    assert!(matches!(
        actor.capture().await,
        Err(CaptureError::Backpressure)
    ));
    assert_eq!(shared.state.lock().unwrap().queued.len(), 1);
    drop(first);
    let second = frame(&actor, &shared).await;
    assert_eq!(second.request().get(), 2);
    drop(second);
    assert!(actor.shutdown().await.unwrap().metadata().is_ok());
}

#[tokio::test]
async fn exported_handles_identify_storage_without_retaining_a_pool_use() {
    let (actor, shared) = fixture(2);
    let handles = actor.buffers().to_vec();
    let first = frame(&actor, &shared).await;
    assert!(handles[0].contains_frame(&first));
    assert!(!handles[1].contains_frame(&first));
    assert_eq!(handles[0].stride(), nz(4));
    let exported = handles[0].as_fd().try_clone_to_owned().unwrap();
    drop(first);
    let second = frame(&actor, &shared).await;
    assert!(handles[0].contains_frame(&second));
    assert_eq!(second.request().get(), 2);
    actor.shutdown().await.unwrap();
    assert!(handles[0].contains_frame(&second));

    let (replacement, shared) = fixture(1);
    let new_frame = frame(&replacement, &shared).await;
    assert!(!handles[0].contains_frame(&new_frame));
    assert!(replacement.buffers()[0].contains_frame(&new_frame));
    drop(new_frame);
    drop(second);
    drop(exported);
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn uncertain_consumer_release_withholds_storage_without_blocking_shutdown() {
    let (actor, shared) = fixture(2);
    let first = frame(&actor, &shared).await;
    first.retire();
    let second = frame(&actor, &shared).await;
    assert!(actor.buffers()[1].contains_frame(&second));
    assert!(matches!(
        actor.capture().await,
        Err(CaptureError::Backpressure)
    ));
    drop(second);
    let next = frame(&actor, &shared).await;
    assert!(actor.buffers()[1].contains_frame(&next));
    next.retire();
    assert!(matches!(
        actor.capture().await,
        Err(CaptureError::Backpressure)
    ));
    actor.shutdown().await.unwrap();
    assert_eq!(shared.state.lock().unwrap().closes, 1);
}

#[tokio::test]
async fn producer_failure_is_not_published_as_pixels() {
    let (actor, shared) = fixture(1);
    let (result, ()) = tokio::join!(actor.capture(), complete(&shared, Err(-nix::libc::EIO)));
    assert!(
        matches!(result, Err(CaptureError::FrameFailed { source, .. }) if source.raw_os_error() == Some(nix::libc::EIO))
    );
    drop(frame(&actor, &shared).await);
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_admission_does_not_consume_a_slot_or_request_name() {
    let (actor, shared) = fixture(1);
    shared.state.lock().unwrap().reject_once = true;
    assert!(matches!(
        actor.capture().await,
        Err(CaptureError::Backpressure)
    ));
    let frame = frame(&actor, &shared).await;
    assert_eq!(frame.request().get(), 1);
    drop(frame);
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn abandoned_capture_is_not_reused_before_completion() {
    let (actor, shared) = fixture(1);
    let mut pending = Box::pin(actor.capture());
    tokio::select! {
        result = &mut pending => panic!("unexpected result: {result:?}"),
        _ = notified(&shared.queued) => (),
    }
    drop(pending);
    assert!(matches!(
        actor.capture().await,
        Err(CaptureError::Backpressure)
    ));
    let request = shared.state.lock().unwrap().queued[0].0;
    shared.state.lock().unwrap().completed.push_back(Completed {
        request,
        outcome: Ok(Duration::ZERO),
    });
    // Wait for the completion to return the abandoned frame, without admitting
    // another request that would need its own completion.
    while !shared.state.lock().unwrap().completed.is_empty() {
        tokio::task::yield_now().await;
    }
    drop(frame(&actor, &shared).await);
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_kernel_retirement_not_consumer_return() {
    let (actor, shared) = fixture(1);
    let frame = frame(&actor, &shared).await;
    shared.state.lock().unwrap().busy = true;
    let mut shutdown = Box::pin(actor.shutdown());
    tokio::select! {
        result = &mut shutdown => panic!("early shutdown: {result:?}"),
        _ = notified(&shared.closing) => (),
    }
    shared.state.lock().unwrap().busy = false;
    shutdown.await.unwrap();
    // The completed frame still retains its allocation after actor shutdown.
    assert!(frame.as_fd().try_clone_to_owned().is_ok());
    drop(frame);
}

#[tokio::test]
async fn shutdown_timeout_never_returns_successful_retirement() {
    let (actor, shared) = fixture(1);
    shared.state.lock().unwrap().busy = true;
    assert_eq!(
        actor.shutdown().await.unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(shared.state.lock().unwrap().closes > 0);
}

#[tokio::test]
async fn unknown_completion_stops_admission() {
    let (actor, shared) = fixture(1);
    shared.state.lock().unwrap().completed.push_back(Completed {
        request: RequestId::new(99).unwrap(),
        outcome: Ok(Duration::ZERO),
    });
    notified(&shared.closing).await;
    assert!(matches!(actor.capture().await, Err(CaptureError::Stopped)));
    assert!(actor.shutdown().await.is_err());
}

#[tokio::test]
async fn independent_destinations_complete_out_of_order() {
    let (actor, shared) = fixture(3);
    let finish = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if shared.state.lock().unwrap().queued.len() == 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let mut state = shared.state.lock().unwrap();
        let queued = state.queued.clone();
        assert_eq!(
            queued.iter().map(|(_, slot)| *slot).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        for (request, _) in queued.into_iter().rev() {
            state.completed.push_back(Completed {
                request,
                outcome: Ok(Duration::from_nanos(request.get())),
            });
        }
    };
    let (first, second, third, ()) =
        tokio::join!(actor.capture(), actor.capture(), actor.capture(), finish);
    let frames = [first.unwrap(), second.unwrap(), third.unwrap()];
    for frame in &frames {
        assert_eq!(frame.timestamp().as_nanos(), frame.request().get() as u128);
    }
    assert!(matches!(
        actor.capture().await,
        Err(CaptureError::Backpressure)
    ));
    drop(frames);
    actor.shutdown().await.unwrap();
}

#[test]
fn configuration_rejects_unbounded_or_unrepresentable_waits() {
    let config = Config {
        capacity: nz(1),
        poll_interval: Duration::from_millis(1),
        shutdown_timeout: Duration::from_secs(1),
    };
    assert!(config.validate(0).is_err());
    assert!(config.validate(65).is_err());
    assert!(Config {
        capacity: nz(2),
        ..config
    }
    .validate(1)
    .is_err());
    assert!(Config {
        poll_interval: Duration::MAX,
        ..config
    }
    .validate(1)
    .is_err());
    assert!(Config {
        shutdown_timeout: Duration::ZERO,
        ..config
    }
    .validate(1)
    .is_err());
}

#[tokio::test]
async fn malformed_completion_errors_stop_without_panicking() {
    for error in [i32::MIN, 0, 5] {
        let (actor, shared) = fixture(1);
        let (result, ()) = tokio::join!(actor.capture(), complete(&shared, Err(error)));
        assert!(matches!(result, Err(CaptureError::Stopped)));
        assert_eq!(
            actor.shutdown().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[tokio::test]
async fn dropping_the_actor_still_closes_its_stream() {
    let (actor, shared) = fixture(1);
    drop(actor);
    notified(&shared.closing).await;
    assert!(shared.state.lock().unwrap().closes > 0);
}

#[tokio::test]
async fn completed_eagain_is_not_admission_backpressure() {
    let (actor, shared) = fixture(1);
    let (result, ()) = tokio::join!(actor.capture(), complete(&shared, Err(-nix::libc::EAGAIN)));
    assert!(
        matches!(result, Err(CaptureError::FrameFailed { request, source })
        if request.get() == 1 && source.raw_os_error() == Some(nix::libc::EAGAIN))
    );
    drop(frame(&actor, &shared).await);
    actor.shutdown().await.unwrap();
}
