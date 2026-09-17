//! Scheduling for one active userspace-rendered output generation.

use std::io;
use std::os::fd::AsFd;
use std::time::Duration;

use pronk_renderer_worker::{DeliveryAttempt, RenderedFrame, SceneAttempt, SceneReader};
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// Render complete scenes and satisfy kernel-issued recipient claims.
pub async fn run_complete_scenes<F: AsFd>(
    reader: SceneReader<F>,
    source_interval: Duration,
    stop: &CancellationToken,
) -> io::Result<()> {
    if source_interval.is_zero() {
        return Err(invalid("renderer source interval is zero"));
    }
    let mut reader = reader;
    let mut current = None;
    let mut source_tick = time::interval(source_interval);
    source_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result = run_until_stopped(&mut reader, &mut current, &mut source_tick, stop).await;
    let retired = current.map_or(Ok(()), |frame| return_frame(&mut reader, frame));
    let result = combine_shutdown(result, retired);
    combine_shutdown(result, reader.withdraw())
}

async fn run_until_stopped<F: AsFd>(
    reader: &mut SceneReader<F>,
    current: &mut Option<RenderedFrame>,
    source_tick: &mut time::Interval,
    stop: &CancellationToken,
) -> io::Result<()> {
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        try_deliver(reader, current)?;
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = source_tick.tick() => {
                if reader.available_slots() == 0 {
                    continue;
                }
                match reader.try_render().map_err(io::Error::other)? {
                    SceneAttempt::NoSlot | SceneAttempt::NoScene => {}
                    SceneAttempt::Rejected { cause } => return Err(cause),
                    SceneAttempt::Rendered(frame) => replace_current(reader, current, frame)?,
                }
            }
        }
    }
}

fn try_deliver<F: AsFd>(
    reader: &mut SceneReader<F>,
    current: &mut Option<RenderedFrame>,
) -> io::Result<()> {
    let Some(frame) = current.take() else {
        return Ok(());
    };
    match reader.try_deliver(frame) {
        Ok(DeliveryAttempt::NoRecipient(frame) | DeliveryAttempt::Delivered(frame)) => {
            *current = Some(frame);
            Ok(())
        }
        Err(error) => {
            let (frame, cause) = error.into_parts();
            if let Some(frame) = frame {
                return_frame(reader, frame)?;
            }
            Err(cause)
        }
    }
}

fn replace_current<F: AsFd>(
    reader: &mut SceneReader<F>,
    current: &mut Option<RenderedFrame>,
    newest: RenderedFrame,
) -> io::Result<()> {
    replace_latest(current, newest, |stale| return_frame(reader, stale))
}

fn replace_latest<T, E>(
    current: &mut Option<T>,
    newest: T,
    retire: impl FnOnce(T) -> Result<(), E>,
) -> Result<(), E> {
    match current.replace(newest) {
        Some(stale) => retire(stale),
        None => Ok(()),
    }
}

fn return_frame<F: AsFd>(reader: &mut SceneReader<F>, frame: RenderedFrame) -> io::Result<()> {
    reader
        .return_frame(frame)
        .map_err(|_| io::Error::other("scene reader rejected its returned private images"))
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

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::{combine_shutdown, replace_latest};
    use std::io;

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

    #[test]
    fn a_new_scene_retires_the_single_private_backlog() {
        let mut current = Some(1);
        let mut retired = None;

        replace_latest(&mut current, 2, |stale| {
            retired = Some(stale);
            Ok::<_, ()>(())
        })
        .unwrap();

        assert_eq!(current, Some(2));
        assert_eq!(retired, Some(1));
    }
}
