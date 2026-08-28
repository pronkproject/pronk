use std::num::{NonZeroU32, NonZeroU64};
use std::time::{Duration, Instant};

use crate::sender_actor::VideoSenderFeedbackSnapshot;
use crate::transport::VideoTransportPressure;

const DEFAULT_BITRATE_FLOOR: u64 = 250_000;
const KEY_FRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
const BITRATE_EVALUATION_INTERVAL: Duration = Duration::from_secs(1);
const BITRATE_RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const LOSS_DECREASE_THRESHOLD: u8 = 26;
const LOSS_RECOVERY_THRESHOLD: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoFeedbackAction {
    ForceKeyFrame,
    SetBitrate(NonZeroU64),
}

/// Generation-local adaptation policy. It contains no transport, GStreamer,
/// actor, or D-Bus handles; callers apply the returned actions at their own
/// layer boundaries.
#[derive(Debug)]
pub(crate) struct VideoFeedbackController {
    ceiling: NonZeroU64,
    floor: NonZeroU64,
    current: NonZeroU64,
    observed_key_frame_requests: u64,
    evaluated_transport_drops: u64,
    last_key_frame_action: Option<Instant>,
    last_bitrate_evaluation: Option<Instant>,
    last_bitrate_change: Option<Instant>,
}

impl VideoFeedbackController {
    pub(crate) fn new(
        configured_bitrate: NonZeroU64,
        receiver_minimum: Option<NonZeroU32>,
    ) -> Self {
        let receiver_minimum = receiver_minimum.map_or(0, |value| u64::from(value.get()));
        let floor = receiver_minimum
            .max(DEFAULT_BITRATE_FLOOR)
            .min(configured_bitrate.get());
        Self {
            ceiling: configured_bitrate,
            floor: NonZeroU64::new(floor).expect("configured bitrate is nonzero"),
            current: configured_bitrate,
            observed_key_frame_requests: 0,
            evaluated_transport_drops: 0,
            last_key_frame_action: None,
            last_bitrate_evaluation: None,
            last_bitrate_change: None,
        }
    }

    pub(crate) fn observe(
        &mut self,
        feedback: VideoSenderFeedbackSnapshot,
        now: Instant,
    ) -> Vec<VideoFeedbackAction> {
        let mut actions = Vec::with_capacity(2);
        if feedback.key_frame_requests > self.observed_key_frame_requests
            && elapsed_at_least(self.last_key_frame_action, now, KEY_FRAME_REQUEST_INTERVAL)
        {
            self.observed_key_frame_requests = feedback.key_frame_requests;
            self.last_key_frame_action = Some(now);
            actions.push(VideoFeedbackAction::ForceKeyFrame);
        }

        if let Some(pressure) = feedback.pressure {
            self.observe_pressure(pressure, now, &mut actions);
        }
        actions
    }

    fn observe_pressure(
        &mut self,
        pressure: VideoTransportPressure,
        now: Instant,
        actions: &mut Vec<VideoFeedbackAction>,
    ) {
        if !elapsed_at_least(
            self.last_bitrate_evaluation,
            now,
            BITRATE_EVALUATION_INTERVAL,
        ) {
            return;
        }
        self.last_bitrate_evaluation = Some(now);
        let new_transport_drop =
            pressure.frames_dropped_or_skipped > self.evaluated_transport_drops;
        self.evaluated_transport_drops = pressure.frames_dropped_or_skipped;
        let overloaded = pressure.queue_saturated()
            || new_transport_drop
            || pressure
                .fraction_lost
                .is_some_and(|loss| loss >= LOSS_DECREASE_THRESHOLD);

        let requested = if overloaded {
            self.decreased_bitrate()
        } else if self.can_recover(pressure, now) {
            self.increased_bitrate()
        } else {
            None
        };
        if let Some(requested) = requested {
            self.current = requested;
            self.last_bitrate_change = Some(now);
            actions.push(VideoFeedbackAction::SetBitrate(requested));
        }
    }

    fn decreased_bitrate(&self) -> Option<NonZeroU64> {
        if self.current <= self.floor {
            return None;
        }
        let decreased = self.current.get().saturating_mul(4) / 5;
        NonZeroU64::new(decreased.max(self.floor.get())).filter(|value| *value < self.current)
    }

    fn can_recover(&self, pressure: VideoTransportPressure, now: Instant) -> bool {
        self.current < self.ceiling
            && elapsed_at_least(self.last_bitrate_change, now, BITRATE_RECOVERY_INTERVAL)
            && !pressure.queue_saturated()
            && pressure
                .in_flight_media_duration
                .checked_mul(2)
                .is_some_and(|duration| duration <= pressure.max_acceptable_in_flight_duration)
            && pressure
                .fraction_lost
                .is_none_or(|loss| loss <= LOSS_RECOVERY_THRESHOLD)
    }

    fn increased_bitrate(&self) -> Option<NonZeroU64> {
        let increment = (self.current.get() / 10).max(100_000);
        NonZeroU64::new(
            self.current
                .get()
                .saturating_add(increment)
                .min(self.ceiling.get()),
        )
        .filter(|value| *value > self.current)
    }
}

fn elapsed_at_least(previous: Option<Instant>, now: Instant, interval: Duration) -> bool {
    previous.is_none_or(|previous| {
        now.checked_duration_since(previous)
            .is_some_and(|elapsed| elapsed >= interval)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_key_frame_requests_and_reduces_at_a_bounded_rate() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            NonZeroU32::new(500_000),
        );
        let overloaded = VideoTransportPressure {
            in_flight_frames: 12,
            in_flight_media_duration: Duration::from_millis(250),
            max_acceptable_in_flight_duration: Duration::from_millis(100),
            ..VideoTransportPressure::default()
        };
        let actions = controller.observe(
            VideoSenderFeedbackSnapshot {
                revision: 1,
                generation: NonZeroU64::new(1),
                key_frame_requests: 3,
                pressure: Some(overloaded),
                ..VideoSenderFeedbackSnapshot::default()
            },
            start,
        );
        assert_eq!(
            actions,
            [
                VideoFeedbackAction::ForceKeyFrame,
                VideoFeedbackAction::SetBitrate(NonZeroU64::new(1_600_000).unwrap()),
            ]
        );

        assert!(controller
            .observe(
                VideoSenderFeedbackSnapshot {
                    revision: 2,
                    generation: NonZeroU64::new(1),
                    key_frame_requests: 4,
                    pressure: Some(overloaded),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + Duration::from_millis(100),
            )
            .is_empty());
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    revision: 3,
                    generation: NonZeroU64::new(1),
                    key_frame_requests: 4,
                    pressure: Some(overloaded),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + KEY_FRAME_REQUEST_INTERVAL,
            ),
            [VideoFeedbackAction::ForceKeyFrame]
        );
    }

    #[test]
    fn recovers_slowly_without_exceeding_the_configured_ceiling() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            NonZeroU32::new(500_000),
        );
        let overloaded = VideoTransportPressure {
            in_flight_media_duration: Duration::from_millis(200),
            max_acceptable_in_flight_duration: Duration::from_millis(100),
            ..VideoTransportPressure::default()
        };
        controller.observe(
            VideoSenderFeedbackSnapshot {
                pressure: Some(overloaded),
                ..VideoSenderFeedbackSnapshot::default()
            },
            start,
        );
        let clear = VideoTransportPressure {
            max_acceptable_in_flight_duration: Duration::from_millis(100),
            ..VideoTransportPressure::default()
        };
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(clear),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + BITRATE_RECOVERY_INTERVAL,
            ),
            [VideoFeedbackAction::SetBitrate(
                NonZeroU64::new(1_760_000).unwrap()
            )]
        );
    }
}
