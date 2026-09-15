//! Scheduling for one active userspace-rendered output generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use pronk_renderer_worker::{
    CompletedOutput, CompletedReturn, ComposedFrame, FinishedOutput, PrivateBuffer, PrivateFrame,
    ReleasedSceneJob, ReleasedSource, SceneAttempt, SceneReader, SourceAttempt, SourceReader,
};
use tokio::task::JoinSet;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{FramePublishError, Video, VideoEvent};

/// Run source staging and output delivery until cancellation or terminal failure.
pub(crate) async fn run<F: AsFd>(
    reader: SourceReader<'_, F>,
    video: &mut Video,
    available: VecDeque<usize>,
    source_interval: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    run_reader(reader, video, available, source_interval, stop).await
}

/// Run complete-scene composition and output delivery until cancellation.
pub async fn run_complete_scenes<F: AsFd>(
    reader: SceneReader<'_, F>,
    video: &mut Video,
    available: VecDeque<usize>,
    source_interval: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    run_reader(reader, video, available, source_interval, stop).await
}

async fn run_reader<R: FrameReader>(
    mut reader: R,
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

async fn run_until_stopped<R: FrameReader>(
    reader: &mut R,
    video: &mut Video,
    pipeline: &mut Pipeline<R::Completed>,
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
            completed = pipeline.frame_tasks.join_next(), if !pipeline.frame_tasks.is_empty() => {
                let completed = completed
                    .ok_or_else(|| io::Error::other("frame completion set ended unexpectedly"))?
                    .map_err(join_error)??;
                let frame = reader.finish(completed)?;
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
                        reader.return_destination(private)?;
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
            _ = source_tick.tick(), if reader.available() != 0 => {
                match reader.try_submit()? {
                    FrameAttempt::Idle => {}
                    FrameAttempt::Rejected(cause) => return Err(cause),
                    FrameAttempt::Submitted(source) => {
                        pipeline.frame_tasks.spawn_blocking(move || R::complete(source));
                    }
                }
            }
        }
    }
}

struct Pipeline<C: Send + 'static> {
    available: VecDeque<usize>,
    frames: VecDeque<PrivateFrame>,
    frame_tasks: JoinSet<io::Result<C>>,
    output_copies: JoinSet<io::Result<CompletedOutput>>,
    producer_waits: JoinSet<FinishedOutput>,
    reader_waits: JoinSet<CompletedReturn>,
    published: bool,
}

impl<C: Send + 'static> Pipeline<C> {
    fn new(available: VecDeque<usize>) -> Self {
        Self {
            available,
            frames: VecDeque::new(),
            frame_tasks: JoinSet::new(),
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

    fn recover_publication<R: FrameReader>(
        &mut self,
        error: FramePublishError,
        reader: &mut R,
    ) -> io::Result<io::Error> {
        let cause = io::Error::new(error.error().kind(), error.error().to_string());
        match error {
            FramePublishError::Prepare(error) => {
                let (private, retirement, _) = error.into_parts();
                if let Some(private) = private {
                    reader.return_destination(private)?;
                }
                if let Some(output) = retirement {
                    self.reader_waits.spawn(async move { output.wait().await });
                }
            }
            FramePublishError::Handoff { private, .. } => {
                reader.return_destination(private)?;
            }
        }
        Ok(cause)
    }

    async fn shutdown(&mut self) {
        stop_tasks(&mut self.frame_tasks).await;
        stop_tasks(&mut self.output_copies).await;
        stop_tasks(&mut self.producer_waits).await;
        stop_tasks(&mut self.reader_waits).await;
    }
}

trait FrameReader {
    type Submitted: Send + 'static;
    type Completed: Send + 'static;

    fn available(&self) -> usize;
    fn try_submit(&mut self) -> io::Result<FrameAttempt<Self::Submitted>>;
    fn complete(submitted: Self::Submitted) -> io::Result<Self::Completed>;
    fn finish(&mut self, completed: Self::Completed) -> io::Result<PrivateFrame>;
    fn return_destination(&mut self, destination: PrivateBuffer) -> io::Result<()>;
}

enum FrameAttempt<S> {
    Idle,
    Rejected(io::Error),
    Submitted(S),
}

impl<F: AsFd> FrameReader for SourceReader<'_, F> {
    type Submitted = ReleasedSource;
    type Completed = PrivateFrame;

    fn available(&self) -> usize {
        self.available_destinations()
    }

    fn try_submit(&mut self) -> io::Result<FrameAttempt<Self::Submitted>> {
        Ok(
            match SourceReader::try_submit(self).map_err(io::Error::other)? {
                SourceAttempt::NoDestination | SourceAttempt::NoSource => FrameAttempt::Idle,
                SourceAttempt::Rejected { cause } => FrameAttempt::Rejected(cause),
                SourceAttempt::Submitted(source) => FrameAttempt::Submitted(source),
            },
        )
    }

    fn complete(submitted: Self::Submitted) -> io::Result<Self::Completed> {
        submitted.wait()
    }

    fn finish(&mut self, completed: Self::Completed) -> io::Result<PrivateFrame> {
        Ok(completed)
    }

    fn return_destination(&mut self, destination: PrivateBuffer) -> io::Result<()> {
        SourceReader::return_destination(self, destination)
            .map_err(|_| io::Error::other("source reader rejected its returned private buffer"))
    }
}

impl<F: AsFd> FrameReader for SceneReader<'_, F> {
    type Submitted = Box<ReleasedSceneJob>;
    type Completed = ComposedFrame;

    fn available(&self) -> usize {
        self.available_slots()
    }

    fn try_submit(&mut self) -> io::Result<FrameAttempt<Self::Submitted>> {
        Ok(
            match SceneReader::try_submit(self).map_err(io::Error::other)? {
                SceneAttempt::NoSlot | SceneAttempt::NoScene => FrameAttempt::Idle,
                SceneAttempt::Rejected { cause } => FrameAttempt::Rejected(cause),
                SceneAttempt::Submitted(scene) => FrameAttempt::Submitted(scene),
            },
        )
    }

    fn complete(submitted: Self::Submitted) -> io::Result<Self::Completed> {
        submitted.compose_and_wait().map_err(io::Error::other)
    }

    fn finish(&mut self, completed: Self::Completed) -> io::Result<PrivateFrame> {
        self.finish_composition(completed).map_err(|_| {
            io::Error::other("scene reader rejected its completed private source stages")
        })
    }

    fn return_destination(&mut self, destination: PrivateBuffer) -> io::Result<()> {
        SceneReader::return_destination(self, destination)
            .map_err(|_| io::Error::other("scene reader rejected its returned final image"))
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
    use pronk_renderer_worker::{CompletedOutput, ComposedFrame, ReleasedSceneJob};
    use std::collections::VecDeque;
    use tokio::sync::oneshot;
    use tokio::task::JoinSet;

    fn assert_send<T: Send>() {}

    #[test]
    fn completed_output_can_return_from_a_blocking_worker() {
        assert_send::<CompletedOutput>();
        assert_send::<ReleasedSceneJob>();
        assert_send::<ComposedFrame>();
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
