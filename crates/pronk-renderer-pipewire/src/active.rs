//! Scheduling for one active userspace-rendered output generation.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use pronk_renderer_worker::{
    CompletedReturn, FinishedOutput, PrivateFrame, SourceAttempt, SourceReader,
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

    loop {
        pipeline.dispatch_outputs(video).await?;
        tokio::select! {
            biased;
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
            completed = pipeline.output_writes.join_next(), if !pipeline.output_writes.is_empty() => {
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
                        return Err(pipeline.recover_publication(error, &mut reader)?);
                    }
                }
            }
            completed = pipeline.output_returns.join_next(), if !pipeline.output_returns.is_empty() => {
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
    output_writes: JoinSet<FinishedOutput>,
    output_returns: JoinSet<CompletedReturn>,
    published: bool,
}

impl Pipeline {
    fn new(available: VecDeque<usize>) -> Self {
        Self {
            available,
            frames: VecDeque::new(),
            source_reads: JoinSet::new(),
            output_writes: JoinSet::new(),
            output_returns: JoinSet::new(),
            published: false,
        }
    }

    async fn dispatch_outputs(&mut self, video: &mut Video) -> io::Result<()> {
        while let Some((slot, frame)) = take_pair(&mut self.available, &mut self.frames) {
            let completed = video.claim(slot)?.copy_from(frame)?;
            let pending = video.submit(completed)?;
            self.output_writes
                .spawn(async move { pending.wait().await });
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
                self.output_returns
                    .spawn(async move { output.wait().await });
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
                    self.output_returns
                        .spawn(async move { output.wait().await });
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
    use super::take_pair;
    use std::collections::VecDeque;

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
}
