//! Scheduling for one active userspace-rendered output generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use pronk_renderer_worker::{
    CompletedOutput, CompletedReturn, FinishedOutput, PrivateFrame, SourceAttempt, SourceReader,
};
use tokio::task::JoinSet;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{FramePublishError, Video, VideoEvent};

/// Run source staging and output delivery until cancellation or terminal failure.
pub(crate) async fn run<F: AsFd>(
    mut reader: SourceReader<'_, F>,
    video: &mut Video,
    available: VecDeque<usize>,
    source_interval: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    if source_interval.is_zero() {
        return Err(invalid("renderer source interval is zero"));
    }
    let mut pipeline = Pipeline::new(available);
    let mut source_tick = time::interval(source_interval);
    source_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result = run_until_stopped(&mut reader, video, &mut pipeline, &mut source_tick, stop).await;
    pipeline.shutdown().await;
    result
}

async fn run_until_stopped<F: AsFd>(
    reader: &mut SourceReader<'_, F>,
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
            completed = pipeline.source_reads.join_next(), if !pipeline.source_reads.is_empty() => {
                let frame = completed
                    .ok_or_else(|| io::Error::other("source wait set ended unexpectedly"))?
                    .map_err(join_error)??;
                pipeline.frames.push_back(frame);
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
                    Ok((private, _)) => {
                        reader.return_destination(private).map_err(|_| {
                            io::Error::other("source reader rejected its returned private buffer")
                        })?;
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
            _ = source_tick.tick(), if reader.available_destinations() != 0 => {
                match reader.try_submit().map_err(io::Error::other)? {
                    SourceAttempt::NoDestination | SourceAttempt::NoSource => {}
                    SourceAttempt::Rejected { cause } => return Err(cause),
                    SourceAttempt::Submitted(source) => {
                        pipeline.source_reads.spawn_blocking(move || source.wait());
                    }
                }
            }
        }
    }
}

struct Pipeline {
    available: VecDeque<usize>,
    frames: VecDeque<PrivateFrame>,
    source_reads: JoinSet<io::Result<PrivateFrame>>,
    output_copies: JoinSet<io::Result<CompletedOutput>>,
    producer_waits: JoinSet<FinishedOutput>,
    reader_waits: JoinSet<CompletedReturn>,
    published: bool,
}

impl Pipeline {
    fn new(available: VecDeque<usize>) -> Self {
        Self {
            available,
            frames: VecDeque::new(),
            source_reads: JoinSet::new(),
            output_copies: JoinSet::new(),
            producer_waits: JoinSet::new(),
            reader_waits: JoinSet::new(),
            published: false,
        }
    }

    fn dispatch_outputs(&mut self, video: &mut Video) -> io::Result<()> {
        while let Some((slot, frame)) = take_pair(&mut self.available, &mut self.frames) {
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
        reader: &mut SourceReader<'_, F>,
    ) -> io::Result<io::Error> {
        let cause = io::Error::new(error.error().kind(), error.error().to_string());
        match error {
            FramePublishError::Prepare(error) => {
                let (private, retirement, _) = error.into_parts();
                if let Some(private) = private {
                    reader.return_destination(private).map_err(|_| {
                        io::Error::other("source reader rejected a recovered private buffer")
                    })?;
                }
                if let Some(output) = retirement {
                    self.reader_waits.spawn(async move { output.wait().await });
                }
            }
            FramePublishError::Handoff { private, .. } => {
                reader.return_destination(private).map_err(|_| {
                    io::Error::other("source reader rejected a recovered private buffer")
                })?;
            }
        }
        Ok(cause)
    }

    async fn shutdown(&mut self) {
        stop_tasks(&mut self.source_reads).await;
        stop_tasks(&mut self.output_copies).await;
        stop_tasks(&mut self.producer_waits).await;
        stop_tasks(&mut self.reader_waits).await;
    }
}

async fn stop_tasks<T: 'static>(tasks: &mut JoinSet<T>) {
    tasks.shutdown().await;
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
    use super::{stop_tasks, take_pair};
    use pronk_renderer_worker::CompletedOutput;
    use std::collections::VecDeque;
    use tokio::sync::oneshot;
    use tokio::task::JoinSet;

    fn assert_send<T: Send>() {}

    #[test]
    fn completed_output_can_return_from_a_blocking_worker() {
        assert_send::<CompletedOutput>();
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

    #[tokio::test]
    async fn stopping_waits_for_a_running_blocking_operation() {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn_blocking(move || {
            let _ = entered.send(());
            let _ = release_rx.blocking_recv();
        });
        entered_rx.await.unwrap();

        let stopping = tokio::spawn(async move {
            stop_tasks(&mut tasks).await;
        });
        tokio::task::yield_now().await;
        assert!(!stopping.is_finished());
        release.send(()).unwrap();
        stopping.await.unwrap();
    }
}
