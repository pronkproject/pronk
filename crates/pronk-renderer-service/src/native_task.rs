//! One renderer generation polls independently of asynchronous service tasks.

use std::future::Future;
use std::io;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Keep synchronous producer waits and native destruction on a dedicated thread.
///
/// The thread stays assigned for the generation's lifetime without occupying
/// the runtime's blocking pool. Async transport still uses the calling runtime's
/// I/O and timer drivers. Cancellation remains cooperative: a native wait must
/// return before that generation can stop. Thread creation failure returns the
/// input without starting the operation or discarding its renderer authority.
pub(crate) fn spawn<I, F, R>(
    input: I,
    operation: F,
) -> Result<JoinHandle<R::Output>, (I, io::Error)>
where
    I: Send + 'static,
    F: FnOnce(I) -> R + Send + 'static,
    R: Future,
    R::Output: Send + 'static,
{
    spawn_with(input, operation, |operation| {
        std::thread::Builder::new()
            .name("pronk-renderer".into())
            .spawn(operation)
    })
}

fn spawn_with<I, F, R>(
    input: I,
    operation: F,
    start: impl FnOnce(
        Box<dyn FnOnce() -> R::Output + Send>,
    ) -> io::Result<std::thread::JoinHandle<R::Output>>,
) -> Result<JoinHandle<R::Output>, (I, io::Error)>
where
    I: Send + 'static,
    F: FnOnce(I) -> R + Send + 'static,
    R: Future,
    R::Output: Send + 'static,
{
    let runtime = Handle::current();
    let (request, receive) = oneshot::channel();
    let (finished, completion) = oneshot::channel::<()>();
    let worker = match start(Box::new(move || {
        // Channel closure also notifies the joiner during panic unwinding.
        let _finished = finished;
        let input = receive.blocking_recv().expect("renderer input was sent");
        runtime.block_on(operation(input))
    })) {
        Ok(worker) => worker,
        Err(error) => return Err((input, error)),
    };
    if let Err(input) = request.send(input) {
        return Err((
            input,
            io::Error::other("renderer thread stopped before startup"),
        ));
    }
    Ok(tokio::spawn(async move {
        let _ = completion.await;
        // A thread may still be destroying thread-local native state after its
        // operation finishes. Join outside the asynchronous executor as well.
        match tokio::task::spawn_blocking(move || worker.join()).await {
            Ok(Ok(output)) => output,
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            Err(error) => panic!("renderer thread join failed: {error}"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    use super::{spawn, spawn_with};

    #[tokio::test(flavor = "current_thread")]
    async fn thread_creation_failure_returns_the_unstarted_input() {
        let input = std::sync::Arc::new(61);
        let original = input.clone();
        let result = spawn_with(
            input,
            |_| async { panic!("failed startup must not run the operation") },
            |_| Err(nix::errno::Errno::EAGAIN.into()),
        );
        let (input, error) = result.unwrap_err();
        assert!(std::sync::Arc::ptr_eq(&input, &original));
        assert_eq!(error.raw_os_error(), Some(nix::errno::Errno::EAGAIN as i32));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_generation_does_not_stall_the_service_runtime() {
        let caller = std::thread::current().id();
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let worker = spawn((), move |()| async move {
            assert_ne!(std::thread::current().id(), caller);
            entered.send(()).unwrap();
            // Bounded even when the spawning implementation regresses.
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            stopped.is_cancelled()
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();

        // Other generations and timer-driven service work continue while the
        // simulated native producer wait is still blocked.
        let other = spawn((), |()| async { 17 }).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), other)
                .await
                .unwrap()
                .unwrap(),
            17
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
        stop.cancel();
        assert!(!worker.is_finished());
        release.send(()).unwrap();
        assert!(worker.await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_generation_can_use_async_timers_and_nested_blocking_work() {
        let worker = spawn((), |()| async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            tokio::task::spawn_blocking(|| 29).await.unwrap()
        })
        .unwrap();
        assert_eq!(worker.await.unwrap(), 29);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_panic_is_reported_through_the_join_handle() {
        let worker = spawn((), |()| async { panic!("native generation failed") }).unwrap();
        assert!(worker.await.unwrap_err().is_panic());
    }

    #[test]
    fn generation_does_not_occupy_the_only_blocking_pool_slot() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let worker = spawn((), |()| async {
                tokio::task::spawn_blocking(|| 41).await.unwrap()
            })
            .unwrap();
            tokio::time::timeout(Duration::from_secs(1), worker).await
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
        assert_eq!(result.unwrap().unwrap(), 41);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn joining_waits_for_native_thread_local_destruction() {
        struct Retirement {
            entered: Option<oneshot::Sender<()>>,
            release: mpsc::Receiver<()>,
        }

        impl Drop for Retirement {
            fn drop(&mut self) {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                let _ = self.release.recv_timeout(Duration::from_secs(5));
            }
        }

        std::thread_local! {
            static RETIREMENT: std::cell::RefCell<Option<Retirement>> =
                const { std::cell::RefCell::new(None) };
        }
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        let worker = spawn((entered, release_rx), |(entered, release)| async move {
            RETIREMENT.with(|retirement| {
                *retirement.borrow_mut() = Some(Retirement {
                    entered: Some(entered),
                    release,
                });
            });
            53
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(!worker.is_finished());
        tokio::time::sleep(Duration::from_millis(1)).await;
        release.send(()).unwrap();
        assert_eq!(worker.await.unwrap(), 53);
    }
}
