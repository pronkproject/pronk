//! Scheduling for one active userspace-rendered output generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use pronk_renderer_worker::{
    CompletedOutput, CompletedReturn, FinishedOutput, RenderedFrame, SceneAttempt, SceneReader,
};
use tokio::task::JoinSet;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{FramePublishError, Video, VideoEvent};

/// Run complete-scene composition and output delivery until cancellation.
pub async fn run_complete_scenes<F: AsFd>(
    reader: SceneReader<F>,
    video: &mut Video,
    available: VecDeque<usize>,
    reader_waits: JoinSet<CompletedReturn>,
    source_interval: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    if source_interval.is_zero() {
        return Err(invalid("renderer source interval is zero"));
    }
    let mut reader = reader;
    let mut pipeline = Pipeline::new(available, reader_waits);
    let mut source_tick = time::interval(source_interval);
    source_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result = run_until_stopped(&mut reader, video, &mut pipeline, &mut source_tick, stop).await;
    let result = combine_shutdown(result, pipeline.shutdown(video, &mut reader).await);
    combine_shutdown(result, reader.withdraw())
}

async fn run_until_stopped<F: AsFd>(
    reader: &mut SceneReader<F>,
    video: &mut Video,
    pipeline: &mut Pipeline,
    source_tick: &mut time::Interval,
    stop: &CancellationToken,
) -> io::Result<()> {
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        pipeline.dispatch_outputs(video)?;
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            event = video.next_event() => {
                if let Some(error) = pipeline.handle_video_event(video, event?).await? {
                    return Err(error);
                }
            }
            completed = pipeline.output_copies.join_next(), if !pipeline.output_copies.is_empty() => {
                let output = completed
                    .ok_or_else(|| io::Error::other("output copy set ended unexpectedly"))?
                    .map_err(join_error)??;
                let pending = video.submit(output)?;
                pipeline.producer_waits.spawn(async move { pending.wait().await });
            }
            completed = pipeline.producer_waits.join_next(), if !pipeline.producer_waits.is_empty() => {
                let output = completed
                    .ok_or_else(|| io::Error::other("output wait set ended unexpectedly"))?
                    .map_err(join_error)?;
                let ready = video.finish(output)?;
                let first = !pipeline.published;
                match video.publish(ready, monotonic_now_ns()?, first).await {
                    Ok((frame, _)) => {
                        return_frame(reader, frame)?;
                        pipeline.published = true;
                    }
                    Err(error) => {
                        return Err(pipeline.recover_publication(error, reader)?);
                    }
                }
            }
            completed = pipeline.reader_waits.join_next(), if !pipeline.reader_waits.is_empty() => {
                let returned = completed
                    .ok_or_else(|| io::Error::other("output return set ended unexpectedly"))?
                    .map_err(join_error)?;
                let slot = video.finish_return(returned)?;
                pipeline.available.push_back(slot);
            }
            _ = source_tick.tick(), if reader.available_slots() != 0 => {
                match SceneReader::try_render(reader).map_err(io::Error::other)? {
                    SceneAttempt::NoSlot | SceneAttempt::NoScene => {}
                    SceneAttempt::Rejected { cause } => return Err(cause),
                    SceneAttempt::Rendered(frame) => {
                        replace_backlog(&mut pipeline.frames, frame, |stale| {
                            return_frame(reader, stale)
                        })?;
                    }
                }
            }
        }
    }
}

struct Pipeline {
    available: VecDeque<usize>,
    frames: VecDeque<RenderedFrame>,
    output_copies: JoinSet<io::Result<CompletedOutput>>,
    producer_waits: JoinSet<FinishedOutput>,
    reader_waits: JoinSet<CompletedReturn>,
    published: bool,
}

impl Pipeline {
    fn new(available: VecDeque<usize>, reader_waits: JoinSet<CompletedReturn>) -> Self {
        Self {
            available,
            frames: VecDeque::new(),
            output_copies: JoinSet::new(),
            producer_waits: JoinSet::new(),
            reader_waits,
            published: false,
        }
    }

    fn dispatch_outputs(&mut self, video: &mut Video) -> io::Result<()> {
        if self.output_copies.is_empty() && self.producer_waits.is_empty() {
            let Some((slot, frame)) = take_pair(&mut self.available, &mut self.frames) else {
                return Ok(());
            };
            let destination = video.claim(slot)?;
            self.output_copies
                .spawn_blocking(move || destination.copy_from(frame));
        }
        Ok(())
    }

    async fn handle_video_event(
        &mut self,
        video: &mut Video,
        event: VideoEvent,
    ) -> io::Result<Option<io::Error>> {
        match event {
            VideoEvent::Ignored => Ok(None),
            VideoEvent::Available { slot } => {
                self.available.push_back(slot);
                Ok(None)
            }
            VideoEvent::Released(output) => {
                self.reader_waits.spawn(async move { output.wait().await });
                Ok(None)
            }
            VideoEvent::Failed { cause, returns } => {
                for output in returns.into_vec() {
                    video.finish_return(output.wait().await)?;
                }
                Ok(Some(io::Error::other(cause.to_string())))
            }
        }
    }

    fn recover_publication<F: AsFd>(
        &mut self,
        error: FramePublishError,
        reader: &mut SceneReader<F>,
    ) -> io::Result<io::Error> {
        let cause = io::Error::new(error.error().kind(), error.error().to_string());
        match error {
            FramePublishError::Prepare(error) => {
                let (frame, retirement, _) = error.into_parts();
                if let Some(frame) = frame {
                    return_frame(reader, frame)?;
                }
                if let Some(output) = retirement {
                    self.reader_waits.spawn(async move { output.wait().await });
                }
            }
            FramePublishError::Handoff { frame, .. } => {
                return_frame(reader, frame)?;
            }
        }
        Ok(cause)
    }

    async fn shutdown<F: AsFd>(
        &mut self,
        video: &mut Video,
        reader: &mut SceneReader<F>,
    ) -> io::Result<()> {
        let mut failure = None;
        while let Some(result) = self.output_copies.join_next().await {
            match result {
                Ok(Ok(output)) => match video.submit(output) {
                    Ok(output) => {
                        let output = output.wait().await;
                        match video
                            .finish(output)
                            .and_then(|output| video.discard(output))
                        {
                            Ok(frame) => {
                                append_shutdown_failure(&mut failure, return_frame(reader, frame))
                            }
                            Err(error) => append_shutdown_failure(&mut failure, Err(error)),
                        }
                    }
                    Err(error) => append_shutdown_failure(&mut failure, Err(error)),
                },
                Ok(Err(error)) => append_shutdown_failure(&mut failure, Err(error)),
                Err(error) => append_shutdown_failure(&mut failure, Err(join_error(error))),
            }
        }
        while let Some(result) = self.producer_waits.join_next().await {
            match result {
                Ok(output) => match video
                    .finish(output)
                    .and_then(|output| video.discard(output))
                {
                    Ok(frame) => append_shutdown_failure(&mut failure, return_frame(reader, frame)),
                    Err(error) => append_shutdown_failure(&mut failure, Err(error)),
                },
                Err(error) => append_shutdown_failure(&mut failure, Err(join_error(error))),
            }
        }
        append_shutdown_failure(
            &mut failure,
            finish_tasks(&mut self.reader_waits, |returned| {
                video.finish_return(returned).map(drop)
            })
            .await,
        );
        while let Some(frame) = self.frames.pop_front() {
            append_shutdown_failure(&mut failure, return_frame(reader, frame));
        }
        failure.map_or(Ok(()), Err)
    }
}

fn return_frame<F: AsFd>(reader: &mut SceneReader<F>, frame: RenderedFrame) -> io::Result<()> {
    SceneReader::return_frame(reader, frame)
        .map_err(|_| io::Error::other("scene reader rejected its returned private images"))
}

async fn finish_tasks<T: 'static>(
    tasks: &mut JoinSet<T>,
    mut finish: impl FnMut(T) -> io::Result<()>,
) -> io::Result<()> {
    let mut failure = None;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(value) => append_shutdown_failure(&mut failure, finish(value)),
            Err(error) => append_shutdown_failure(&mut failure, Err(join_error(error))),
        }
    }
    failure.map_or(Ok(()), Err)
}

fn combine_shutdown(run: io::Result<()>, shutdown: io::Result<()>) -> io::Result<()> {
    match (run, shutdown) {
        (Ok(()), shutdown) => shutdown,
        (run, Ok(())) => run,
        (Err(primary), Err(shutdown)) => Err(io::Error::new(
            primary.kind(),
            format!("{primary}; finish renderer operations: {shutdown}"),
        )),
    }
}

fn append_shutdown_failure(failure: &mut Option<io::Error>, result: io::Result<()>) {
    let Err(error) = result else {
        return;
    };
    *failure = Some(match failure.take() {
        Some(primary) => io::Error::new(primary.kind(), format!("{primary}; {error}")),
        None => error,
    });
}

fn take_pair<L, R>(left: &mut VecDeque<L>, right: &mut VecDeque<R>) -> Option<(L, R)> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some((
        left.pop_front().expect("checked left queue"),
        right.pop_front().expect("checked right queue"),
    ))
}

fn replace_backlog<T, E>(
    queue: &mut VecDeque<T>,
    newest: T,
    mut retire: impl FnMut(T) -> Result<(), E>,
) -> Result<(), E> {
    // No destination has been claimed for these frames. Keep one recent scene
    // while output is busy instead of replaying a burst after backpressure.
    while let Some(stale) = queue.pop_front() {
        retire(stale)?;
    }
    queue.push_back(newest);
    Ok(())
}

fn monotonic_now_ns() -> io::Result<i64> {
    Ok(clock_gettime(ClockId::CLOCK_MONOTONIC)?.num_nanoseconds())
}

fn join_error(error: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("join renderer operation: {error}"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::{combine_shutdown, finish_tasks, replace_backlog, take_pair};
    use pronk_renderer_worker::{CompletedOutput, RenderedFrame};
    use std::collections::VecDeque;
    use std::io;
    use tokio::sync::oneshot;
    use tokio::task::JoinSet;

    fn assert_send<T: Send>() {}

    #[test]
    fn completed_output_can_return_from_a_blocking_worker() {
        assert_send::<CompletedOutput>();
        assert_send::<RenderedFrame>();
    }

    #[test]
    fn pairing_does_not_discard_an_unmatched_item() {
        let mut left = VecDeque::from([1]);
        let mut right: VecDeque<i32> = VecDeque::new();
        assert_eq!(take_pair(&mut left, &mut right), None);
        assert_eq!(left, [1]);

        let mut left: VecDeque<i32> = VecDeque::new();
        let mut right = VecDeque::from([2]);
        assert_eq!(take_pair(&mut left, &mut right), None);
        assert_eq!(right, [2]);
    }

    #[test]
    fn newest_unbound_frame_replaces_the_private_backlog() {
        let mut frames = VecDeque::from([1, 2]);
        let mut retired = Vec::new();

        replace_backlog(&mut frames, 3, |frame| {
            retired.push(frame);
            Ok::<_, ()>(())
        })
        .unwrap();

        assert_eq!(retired, [1, 2]);
        assert_eq!(frames, [3]);
    }

    #[tokio::test]
    async fn finishing_waits_for_a_running_blocking_operation() {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn_blocking(move || {
            let _ = entered.send(());
            let _ = release_rx.blocking_recv();
        });
        entered_rx.await.unwrap();

        let stopping =
            tokio::spawn(async move { finish_tasks(&mut tasks, |_| Ok(())).await.unwrap() });
        tokio::task::yield_now().await;
        assert!(!stopping.is_finished());
        release.send(()).unwrap();
        stopping.await.unwrap();
    }

    #[tokio::test]
    async fn finishing_does_not_cancel_an_async_wait() {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = entered.send(());
            let _ = release_rx.await;
        });
        entered_rx.await.unwrap();

        let finishing =
            tokio::spawn(async move { finish_tasks(&mut tasks, |_| Ok(())).await.unwrap() });
        tokio::task::yield_now().await;
        assert!(!finishing.is_finished());
        release.send(()).unwrap();
        finishing.await.unwrap();
    }

    #[tokio::test]
    async fn finishing_applies_every_completed_result() {
        let mut tasks = JoinSet::new();
        tasks.spawn(async { 1 });
        tasks.spawn(async { 2 });
        let mut finished = Vec::new();

        let error = finish_tasks(&mut tasks, |value| {
            finished.push(value);
            Err(io::Error::other(format!("failed result {value}")))
        })
        .await
        .unwrap_err();

        finished.sort_unstable();
        assert_eq!(finished, [1, 2]);
        assert!(error.to_string().contains("failed result 1"));
        assert!(error.to_string().contains("failed result 2"));
    }

    #[test]
    fn shutdown_failure_preserves_the_primary_error_class() {
        let primary = io::Error::new(io::ErrorKind::BrokenPipe, "renderer failed");
        let shutdown = io::Error::other("native copy failed");

        let combined = combine_shutdown(Err(primary), Err(shutdown)).unwrap_err();
        assert_eq!(combined.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            combined.to_string(),
            "renderer failed; finish renderer operations: native copy failed"
        );
    }
}
