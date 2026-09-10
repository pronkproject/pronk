//! Diagnostic host durations, not GPU timestamps or delivered frame cadence.

use std::time::Duration;

#[derive(Default)]
pub struct Timings {
    // Includes fixture generation, native producer waits and read submission.
    pub submission: Duration,
    // Remaining read wait after the coordinator collects accounting.
    pub retirement: Duration,
    pub composition: Duration,
    pub color: Duration,
    pub output: Duration,
    // Fixture-only source and composed-image corruption controls.
    pub overwrites: Duration,
}

#[derive(Default)]
pub struct Report {
    frames: Vec<Timings>,
}

impl Report {
    pub fn push(&mut self, timings: Timings) {
        self.frames.push(timings);
    }

    pub fn print(&self) {
        eprintln!("Renderer timings: {} frames, host wall-clock microseconds; not GPU timestamps or delivered cadence", self.frames.len());
        for (name, select) in [
            (
                "generated-source-submit",
                (|t: &Timings| t.submission) as fn(&Timings) -> Duration,
            ),
            ("remaining-source-wait", |t: &Timings| t.retirement),
            ("private-composition", |t: &Timings| t.composition),
            ("private-gamma", |t: &Timings| t.color),
            ("shared-output-copy", |t: &Timings| t.output),
            ("test-overwrites", |t: &Timings| t.overwrites),
        ] {
            if let Some([p50, p95, max]) = distribution(self.frames.iter().map(select)) {
                eprintln!(
                    "  {name}: p50={} p95={} max={}",
                    p50.as_micros(),
                    p95.as_micros(),
                    max.as_micros()
                );
            }
        }
    }
}

fn distribution(samples: impl Iterator<Item = Duration>) -> Option<[Duration; 3]> {
    let mut samples: Vec<_> = samples.collect();
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let count = samples.len();
    // Nearest-rank quantiles: ceil(p * count) - 1, without multiplication.
    Some([
        samples[(count - 1) / 2],
        samples[count - count / 20 - 1],
        samples[count - 1],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_timing_has_no_quantiles() {
        assert_eq!(distribution(std::iter::empty()), None);
    }

    #[test]
    fn one_or_repeated_sample_keeps_its_duration() {
        for count in [1, 2, 20] {
            let value = Duration::from_nanos(1537);
            assert_eq!(
                distribution(std::iter::repeat_n(value, count)),
                Some([value; 3])
            );
        }
    }

    #[test]
    fn quantiles_use_sorted_nearest_ranks() {
        let ms = Duration::from_millis;
        assert_eq!(
            distribution((1..=20).rev().map(ms)),
            Some([ms(10), ms(19), ms(20)])
        );
        assert_eq!(
            distribution((1..=21).rev().map(ms)),
            Some([ms(11), ms(20), ms(21)])
        );
        assert_eq!(
            distribution([ms(9), ms(1)].into_iter()),
            Some([ms(1), ms(9), ms(9)])
        );
    }
}
