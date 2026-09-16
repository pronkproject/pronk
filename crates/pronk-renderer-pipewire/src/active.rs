//! Scheduling for one active userspace-rendered output generation.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use pronk_renderer_worker::{
    CompletedOutput, CompletedReturn, ComposedFrame, FinishedOutput, PrivateBuffer, PrivateFrame,
    SceneAttempt, SceneReader,
};
use tokio::task::JoinSet;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{FramePublishError, Video, VideoEvent};

/// Run complete-scene composition and output delivery until cancellation.
pub async fn run_complete_scenes<F: AsFd>(
    reader: SceneReader<'_, F>,
    video: &mut Video,
    available: VecDeque<usize>,
    source_interval: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    if source_interval.is_zero() {
        return Err(invalid("renderer source interval is zero"));
    }
    let mut reader = reader;
    let mut pipeline = Pipeline::new(available);
    let mut source_tick = time::interval(source_interval);
    source_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result = run_until_stopped(&mut reader, video, &mut pipeline, &mut source_tick, stop).await;
    pipeline.shutdown().await;
    result
}

async fn run_until_stopped<F: AsFd>(
    reader: &mut SceneReader<'_, F>,
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
            completed = pipeline.frame_tasks.join_next(), if !pipeline.frame_tasks.is_empty() => {
                let (sequence, completed) = completed
                    .ok_or_else(|| io::Error::other("frame completion set ended unexpectedly"))?
                    .map_err(join_error)?;
                for completed in pipeline.completion_order.complete(sequence, completed?)? {
                    let frame = reader.finish_composition(completed).map_err(|_| {
                        io::Error::other(
                            "scene reader rejected its completed private source stages",
                        )
                    })?;
                    pipeline.frames.push_back(frame);
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
                    Ok((private, _)) => {
                        return_destination(reader, private)?;
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
                match SceneReader::try_submit(reader).map_err(io::Error::other)? {
                    SceneAttempt::NoSlot | SceneAttempt::NoScene => {}
                    SceneAttempt::Rejected { cause } => return Err(cause),
                    SceneAttempt::Submitted(scene) => {
                        let sequence = pipeline.completion_order.admit()?;
                        pipeline.frame_tasks.spawn_blocking(move || {
                            (sequence, scene.compose_and_wait().map_err(io::Error::other))
                        });
                    }
                }
            }
        }
    }
}

struct Pipeline {
    available: VecDeque<usize>,
    frames: VecDeque<PrivateFrame>,
    frame_tasks: JoinSet<(u64, io::Result<ComposedFrame>)>,
    completion_order: CompletionOrder<ComposedFrame>,
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
            frame_tasks: JoinSet::new(),
            completion_order: CompletionOrder::new(),
            output_copies: JoinSet::new(),
            producer_waits: JoinSet::new(),
            reader_waits: JoinSet::new(),
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
        reader: &mut SceneReader<'_, F>,
    ) -> io::Result<io::Error> {
        let cause = io::Error::new(error.error().kind(), error.error().to_string());
        match error {
            FramePublishError::Prepare(error) => {
                let (private, retirement, _) = error.into_parts();
                if let Some(private) = private {
                    return_destination(reader, private)?;
                }
                if let Some(output) = retirement {
                    self.reader_waits.spawn(async move { output.wait().await });
                }
            }
            FramePublishError::Handoff { private, .. } => {
                return_destination(reader, private)?;
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

struct CompletionOrder<C> {
    next_admission: u64,
    next_delivery: u64,
    completed: BTreeMap<u64, C>,
}

impl<C> CompletionOrder<C> {
    fn new() -> Self {
        Self {
            next_admission: 0,
            next_delivery: 0,
            completed: BTreeMap::new(),
        }
    }

    fn admit(&mut self) -> io::Result<u64> {
        let sequence = self.next_admission;
        self.next_admission = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("frame admission sequence overflowed"))?;
        Ok(sequence)
    }

    fn complete(&mut self, sequence: u64, completed: C) -> io::Result<Vec<C>> {
        if sequence < self.next_delivery
            || sequence >= self.next_admission
            || self.completed.contains_key(&sequence)
        {
            return Err(io::Error::other("invalid frame completion sequence"));
        }
        self.completed.insert(sequence, completed);
        let mut ready = Vec::new();
        ready
            .try_reserve_exact(self.completed.len())
            .map_err(io::Error::other)?;
        while let Some(completed) = self.completed.remove(&self.next_delivery) {
            ready.push(completed);
            self.next_delivery = self
                .next_delivery
                .checked_add(1)
                .ok_or_else(|| io::Error::other("frame delivery sequence overflowed"))?;
        }
        Ok(ready)
    }
}

fn return_destination<F: AsFd>(
    reader: &mut SceneReader<'_, F>,
    destination: PrivateBuffer,
) -> io::Result<()> {
    SceneReader::return_destination(reader, destination)
        .map_err(|_| io::Error::other("scene reader rejected its returned final image"))
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
    use super::{stop_tasks, take_pair, CompletionOrder};
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

    #[test]
    fn completed_frames_return_in_admission_order() {
        let mut order = CompletionOrder::new();
        let first = order.admit().unwrap();
        let second = order.admit().unwrap();
        let third = order.admit().unwrap();

        assert!(order.complete(third, 30).unwrap().is_empty());
        assert_eq!(order.complete(first, 10).unwrap(), [10]);
        assert_eq!(order.complete(second, 20).unwrap(), [20, 30]);
        assert!(order.complete(second, 99).is_err());

        let fourth = order.admit().unwrap();
        let fifth = order.admit().unwrap();
        assert!(order.complete(fifth, 50).unwrap().is_empty());
        assert!(order.complete(fifth, 55).is_err());
        assert_eq!(order.complete(fourth, 40).unwrap(), [40, 50]);
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
