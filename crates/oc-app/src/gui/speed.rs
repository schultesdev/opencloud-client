//! Transfer speed estimated from the byte counts of successive progress
//! events — for the whole pass and for each running file.

use std::time::Duration;
use std::time::Instant;

/// Minimum spacing of two samples that feed the estimate; closer events
/// carry too few bytes to say anything about the rate.
const MIN_SAMPLE_GAP: Duration = Duration::from_millis(50);

/// A smoothed bytes-per-second estimate (exponential moving average).
#[derive(Debug, Clone, Default)]
pub(super) struct SpeedEstimate {
    /// Byte count and arrival time of the last sample.
    last: Option<(u64, Instant)>,
    /// The smoothed speed in bytes per second.
    bps: f64,
}

impl SpeedEstimate {
    /// Feeds the cumulative byte count reported at `now`. Only rising
    /// samples move the estimate; a regression (a retried transfer
    /// restarts its count at 0) merely re-bases it — the link speed has
    /// not changed, so the estimate is kept.
    pub(super) fn observe(&mut self, bytes: u64, now: Instant) {
        let Some((previous, at)) = self.last else {
            self.last = Some((bytes, now));
            return;
        };
        if bytes < previous {
            self.last = Some((bytes, now));
            return;
        }
        let elapsed = now.saturating_duration_since(at);
        if elapsed < MIN_SAMPLE_GAP {
            return;
        }
        let gain = bytes - previous;
        if gain > 0 {
            let rate = gain as f64 / elapsed.as_secs_f64();
            self.bps = if self.bps == 0.0 {
                rate
            } else {
                // New samples shift the average by 40%.
                self.bps * 0.6 + rate * 0.4
            };
        }
        self.last = Some((bytes, now));
    }

    /// The current estimate in bytes per second (0 until two samples
    /// arrived).
    pub(super) fn bps(&self) -> f64 {
        self.bps
    }

    /// How long `remaining` bytes take at the current speed; `None`
    /// while the speed is unknown (below 1 B/s).
    pub(super) fn eta(&self, remaining: u64) -> Option<Duration> {
        (self.bps >= 1.0).then(|| Duration::from_secs_f64(remaining as f64 / self.bps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn first_sample_only_sets_the_base() {
        let start = Instant::now();
        let mut speed = SpeedEstimate::default();
        speed.observe(1000, start);
        assert_eq!(speed.bps(), 0.0);
        assert_eq!(speed.eta(500), None);
    }

    #[test]
    fn two_samples_yield_a_rate_and_an_eta() {
        let start = Instant::now();
        let mut speed = SpeedEstimate::default();
        speed.observe(0, start);
        speed.observe(1000, at(start, 1000));
        assert_eq!(speed.bps(), 1000.0);
        assert_eq!(speed.eta(2000), Some(Duration::from_secs(2)));
    }

    #[test]
    fn later_samples_are_smoothed() {
        let start = Instant::now();
        let mut speed = SpeedEstimate::default();
        speed.observe(0, start);
        speed.observe(1000, at(start, 1000));
        speed.observe(1000 + 3000, at(start, 2000));
        // 0.6 * 1000 + 0.4 * 3000
        assert_eq!(speed.bps(), 1800.0);
    }

    #[test]
    fn regression_keeps_the_speed_and_rebases() {
        let start = Instant::now();
        let mut speed = SpeedEstimate::default();
        speed.observe(0, start);
        speed.observe(1000, at(start, 1000));
        // The upload was retried: its count starts over.
        speed.observe(0, at(start, 1500));
        assert_eq!(speed.bps(), 1000.0);
        speed.observe(1000, at(start, 2500));
        assert_eq!(speed.bps(), 1000.0, "the rebased sample measures 1000 B/s");
    }

    #[test]
    fn samples_too_close_together_are_ignored() {
        let start = Instant::now();
        let mut speed = SpeedEstimate::default();
        speed.observe(0, start);
        speed.observe(5000, at(start, 10));
        assert_eq!(speed.bps(), 0.0);
        speed.observe(1000, at(start, 1000));
        assert_eq!(speed.bps(), 1000.0, "measured against the kept base");
    }
}
