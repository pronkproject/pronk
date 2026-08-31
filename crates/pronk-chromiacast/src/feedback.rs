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
const PLAYOUT_DELAY_STEP: Duration = Duration::from_millis(33);
const MAXIMUM_PLAYOUT_DELAY: Duration = Duration::from_millis(250);
const PLAYOUT_ADJUSTMENT_INTERVAL: Duration = Duration::from_secs(1);
const PLAYOUT_UPDATE_RETRY_INTERVAL: Duration = Duration::from_secs(2);
pub(crate) const MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS: u8 = 3;
const PLAYOUT_RECOVERY_INTERVAL: Duration = Duration::from_secs(15);
const RTT_PLAYOUT_MULTIPLIER: u32 = 3;

pub(crate) const INITIAL_PLAYOUT_DELAY: Duration = Duration::from_millis(66);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdaptivePlayoutDelayConfiguration {
    pub minimum: Duration,
    pub initial: Duration,
    pub receiver_maximum: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoFeedbackAction {
    ForceKeyFrame,
    SetBitrate(NonZeroU64),
    SetPlayoutDelay(Duration),
    DisableAdaptivePlayoutDelay {
        requested: Duration,
        receiver: Option<Duration>,
    },
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
    playout_delay: Option<AdaptivePlayoutDelayController>,
}

#[derive(Debug)]
struct AdaptivePlayoutDelayController {
    minimum: Duration,
    maximum: Duration,
    current: Duration,
    evaluated_nack_count: u64,
    evaluated_transport_drops: u64,
    step_increase_pending: bool,
    confirmed: bool,
    stable_since: Option<Instant>,
    last_adjustment: Option<Instant>,
    last_update_sent: Option<Instant>,
    update_attempts: u8,
    updates_enabled: bool,
}

impl VideoFeedbackController {
    pub(crate) fn new(
        configured_bitrate: NonZeroU64,
        receiver_minimum: Option<NonZeroU32>,
        playout_delay: Option<AdaptivePlayoutDelayConfiguration>,
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
            playout_delay: playout_delay.map(AdaptivePlayoutDelayController::new),
        }
    }

    pub(crate) fn observe(
        &mut self,
        feedback: VideoSenderFeedbackSnapshot,
        now: Instant,
    ) -> Vec<VideoFeedbackAction> {
        let mut actions = Vec::with_capacity(3);
        if feedback.key_frame_requests > self.observed_key_frame_requests
            && elapsed_at_least(self.last_key_frame_action, now, KEY_FRAME_REQUEST_INTERVAL)
        {
            self.observed_key_frame_requests = feedback.key_frame_requests;
            self.last_key_frame_action = Some(now);
            actions.push(VideoFeedbackAction::ForceKeyFrame);
        }

        if let Some(pressure) = feedback.pressure {
            self.observe_pressure(pressure, now, &mut actions);
            if let Some(playout_delay) = &mut self.playout_delay {
                if let Some(action) = playout_delay.observe(pressure, now) {
                    actions.push(action);
                }
            }
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

impl AdaptivePlayoutDelayController {
    fn new(configuration: AdaptivePlayoutDelayConfiguration) -> Self {
        let minimum = configuration.minimum.min(configuration.initial);
        let maximum = configuration
            .receiver_maximum
            .unwrap_or(MAXIMUM_PLAYOUT_DELAY)
            .min(MAXIMUM_PLAYOUT_DELAY)
            .max(configuration.initial);
        Self {
            minimum,
            maximum,
            current: configuration.initial,
            evaluated_nack_count: 0,
            evaluated_transport_drops: 0,
            step_increase_pending: false,
            confirmed: false,
            stable_since: None,
            last_adjustment: None,
            last_update_sent: None,
            update_attempts: 0,
            updates_enabled: true,
        }
    }

    fn observe(
        &mut self,
        pressure: VideoTransportPressure,
        now: Instant,
    ) -> Option<VideoFeedbackAction> {
        if !self.updates_enabled {
            return None;
        }
        let new_nack = pressure.nack_count > self.evaluated_nack_count;
        self.evaluated_nack_count = pressure.nack_count;
        let new_transport_drop =
            pressure.frames_dropped_or_skipped > self.evaluated_transport_drops;
        self.evaluated_transport_drops = pressure.frames_dropped_or_skipped;
        let required_for_rtt = pressure
            .current_rtt
            .map(|rtt| whole_milliseconds(rtt.saturating_mul(RTT_PLAYOUT_MULTIPLIER)))
            .unwrap_or(self.minimum)
            .clamp(self.minimum, self.maximum);
        let step_pressure = new_nack || new_transport_drop;
        let deadline_pressure = step_pressure || required_for_rtt > self.current;
        let lossy = pressure
            .fraction_lost
            .is_some_and(|loss| loss > LOSS_RECOVERY_THRESHOLD);

        if deadline_pressure || lossy {
            self.stable_since = None;
        }
        self.step_increase_pending |= step_pressure;

        if pressure.receiver_playout_delay != Some(self.current) {
            self.confirmed = false;
            self.stable_since = None;
            let last_update_sent = self.last_update_sent.get_or_insert(now);
            if now
                .checked_duration_since(*last_update_sent)
                .is_some_and(|elapsed| elapsed >= PLAYOUT_UPDATE_RETRY_INTERVAL)
            {
                *last_update_sent = now;
                if self.update_attempts >= MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS {
                    self.updates_enabled = false;
                    return Some(VideoFeedbackAction::DisableAdaptivePlayoutDelay {
                        requested: self.current,
                        receiver: pressure.receiver_playout_delay,
                    });
                }
                self.update_attempts += 1;
                return Some(VideoFeedbackAction::SetPlayoutDelay(self.current));
            }
            return None;
        }
        if !self.confirmed {
            self.confirmed = true;
            self.last_update_sent = None;
            self.update_attempts = 0;
        }
        if !deadline_pressure && !lossy {
            self.stable_since.get_or_insert(now);
        }
        if (self.step_increase_pending || required_for_rtt > self.current)
            && elapsed_at_least(self.last_adjustment, now, PLAYOUT_ADJUSTMENT_INTERVAL)
        {
            let requested = (if self.step_increase_pending {
                self.current.saturating_add(PLAYOUT_DELAY_STEP)
            } else {
                self.current
            })
            .max(required_for_rtt)
            .min(self.maximum);
            self.step_increase_pending = false;
            if requested > self.current {
                return Some(self.set_current(requested, now));
            }
        }
        if self.current > self.minimum
            && !self.step_increase_pending
            && !lossy
            && elapsed_at_least(self.stable_since, now, PLAYOUT_RECOVERY_INTERVAL)
        {
            let requested = self
                .current
                .saturating_sub(PLAYOUT_DELAY_STEP)
                .max(required_for_rtt)
                .max(self.minimum);
            if requested < self.current {
                return Some(self.set_current(requested, now));
            }
        }
        None
    }

    fn set_current(&mut self, requested: Duration, now: Instant) -> VideoFeedbackAction {
        self.current = requested;
        self.confirmed = false;
        self.stable_since = None;
        self.last_adjustment = Some(now);
        self.last_update_sent = Some(now);
        self.update_attempts = 1;
        VideoFeedbackAction::SetPlayoutDelay(requested)
    }
}

fn whole_milliseconds(duration: Duration) -> Duration {
    let milliseconds = duration
        .as_secs()
        .saturating_mul(1_000)
        .saturating_add(u64::from(duration.subsec_millis()))
        .saturating_add(u64::from(duration.subsec_nanos() % 1_000_000 != 0));
    Duration::from_millis(milliseconds)
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
            None,
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
            None,
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

    #[test]
    fn raises_delay_on_retransmission_and_recovers_only_after_stability() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            None,
            Some(AdaptivePlayoutDelayConfiguration {
                minimum: Duration::from_millis(17),
                initial: INITIAL_PLAYOUT_DELAY,
                receiver_maximum: None,
            }),
        );
        let mut pressure = VideoTransportPressure {
            receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
            nack_count: 1,
            ..VideoTransportPressure::default()
        };

        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                99
            ))]
        );
        assert!(controller
            .observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + Duration::from_secs(1),
            )
            .is_empty());

        pressure.receiver_playout_delay = Some(Duration::from_millis(99));
        assert!(controller
            .observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + Duration::from_secs(2),
            )
            .is_empty());
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + Duration::from_secs(2) + PLAYOUT_RECOVERY_INTERVAL,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(INITIAL_PLAYOUT_DELAY)]
        );
    }

    #[test]
    fn stable_link_reduces_the_starting_delay_to_one_frame() {
        let start = Instant::now();
        let minimum = Duration::from_millis(17);
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            None,
            Some(AdaptivePlayoutDelayConfiguration {
                minimum,
                initial: INITIAL_PLAYOUT_DELAY,
                receiver_maximum: None,
            }),
        );
        let pressure = VideoTransportPressure {
            receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
            ..VideoTransportPressure::default()
        };

        assert!(controller
            .observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start,
            )
            .is_empty());
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + PLAYOUT_RECOVERY_INTERVAL,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(minimum)]
        );
    }

    #[test]
    fn retries_an_unconfirmed_delay_without_stacking_an_increase() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            None,
            Some(AdaptivePlayoutDelayConfiguration {
                minimum: Duration::from_millis(17),
                initial: INITIAL_PLAYOUT_DELAY,
                receiver_maximum: None,
            }),
        );
        let pressure = VideoTransportPressure {
            receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
            nack_count: 1,
            ..VideoTransportPressure::default()
        };
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                66
            ))]
        );
        assert!(controller
            .observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + Duration::from_secs(1),
            )
            .is_empty());
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + PLAYOUT_UPDATE_RETRY_INTERVAL,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                66
            ))]
        );
    }

    #[test]
    fn disables_adaptation_when_the_receiver_ignores_bounded_retries() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            None,
            Some(AdaptivePlayoutDelayConfiguration {
                minimum: Duration::from_millis(17),
                initial: INITIAL_PLAYOUT_DELAY,
                receiver_maximum: None,
            }),
        );
        let mut pressure = VideoTransportPressure {
            receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
            nack_count: 1,
            ..VideoTransportPressure::default()
        };

        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                99
            ))]
        );
        for attempt in 1..MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS {
            assert_eq!(
                controller.observe(
                    VideoSenderFeedbackSnapshot {
                        pressure: Some(pressure),
                        ..VideoSenderFeedbackSnapshot::default()
                    },
                    start + PLAYOUT_UPDATE_RETRY_INTERVAL * u32::from(attempt),
                ),
                [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                    99
                ))]
            );
        }
        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + PLAYOUT_UPDATE_RETRY_INTERVAL * u32::from(MAXIMUM_PLAYOUT_UPDATE_ATTEMPTS),
            ),
            [VideoFeedbackAction::DisableAdaptivePlayoutDelay {
                requested: Duration::from_millis(99),
                receiver: Some(INITIAL_PLAYOUT_DELAY),
            }]
        );

        pressure.nack_count += 1;
        assert!(controller
            .observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start + Duration::from_secs(60),
            )
            .is_empty());
    }

    #[test]
    fn rtt_headroom_is_rounded_and_receiver_bounded() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            None,
            Some(AdaptivePlayoutDelayConfiguration {
                minimum: Duration::from_millis(17),
                initial: INITIAL_PLAYOUT_DELAY,
                receiver_maximum: Some(Duration::from_millis(80)),
            }),
        );
        let pressure = VideoTransportPressure {
            current_rtt: Some(Duration::from_micros(30_100)),
            receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
            ..VideoTransportPressure::default()
        };

        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                80
            ))]
        );
    }

    #[test]
    fn rtt_headroom_does_not_take_a_full_loss_recovery_step() {
        let start = Instant::now();
        let mut controller = VideoFeedbackController::new(
            NonZeroU64::new(2_000_000).unwrap(),
            None,
            Some(AdaptivePlayoutDelayConfiguration {
                minimum: Duration::from_millis(17),
                initial: INITIAL_PLAYOUT_DELAY,
                receiver_maximum: None,
            }),
        );
        let pressure = VideoTransportPressure {
            current_rtt: Some(Duration::from_millis(12)),
            receiver_playout_delay: Some(INITIAL_PLAYOUT_DELAY),
            ..VideoTransportPressure::default()
        };

        assert_eq!(
            controller.observe(
                VideoSenderFeedbackSnapshot {
                    pressure: Some(pressure),
                    ..VideoSenderFeedbackSnapshot::default()
                },
                start,
            ),
            [VideoFeedbackAction::SetPlayoutDelay(Duration::from_millis(
                36
            ))]
        );
    }
}
