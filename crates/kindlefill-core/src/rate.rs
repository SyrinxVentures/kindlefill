//! Transfer-rate estimation and the progress figures a UI renders.
//!
//! Pure and clock-free — elapsed time is passed in rather than read — so the awkward
//! cases can be tested directly. Those cases are most of the value here: a progress
//! bar that shows `NaN%`, jumps past 100%, or claims 3 million hours remaining is
//! worse than no progress bar, and every one of those comes from arithmetic that
//! looks obviously fine until a real device does something unexpected.

use std::time::Duration;

/// Beyond this we stop pretending the estimate means anything, and it keeps
/// `Duration::from_secs_f64` away from its overflow panic.
const MAX_ETA: Duration = Duration::from_secs(99 * 3600);

/// Exponentially-smoothed transfer rate in bytes per second.
///
/// Smoothed rather than averaged over the whole run because MTP throughput is lumpy:
/// the device commits an object, pauses, and starts the next. A plain average lets an
/// early stall distort the estimate for the rest of a 20-minute transfer.
#[derive(Debug, Clone)]
pub struct RateEstimator {
    ema: Option<f64>,
    alpha: f64,
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RateEstimator {
    /// Weight of each new sample. Low enough to ride out per-object commit pauses,
    /// high enough that the estimate still tracks a genuine slowdown.
    pub const DEFAULT_ALPHA: f64 = 0.15;

    #[must_use]
    pub fn new() -> Self {
        Self::with_alpha(Self::DEFAULT_ALPHA)
    }

    #[must_use]
    pub fn with_alpha(alpha: f64) -> Self {
        Self {
            ema: None,
            alpha: alpha.clamp(0.01, 1.0),
        }
    }

    /// Record that `bytes` moved in `elapsed`.
    ///
    /// Samples carrying no information are dropped rather than folded in: a zero
    /// duration would divide by zero, and a zero-byte sample would drag the estimate
    /// toward nothing during an ordinary between-object pause.
    pub fn observe(&mut self, bytes: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        if bytes == 0 || secs <= 0.0 {
            return;
        }
        let sample = bytes as f64 / secs;
        if !sample.is_finite() {
            return;
        }
        self.ema = Some(match self.ema {
            None => sample,
            Some(prev) => self.alpha * sample + (1.0 - self.alpha) * prev,
        });
    }

    /// Bytes per second, or `None` before any usable sample.
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        self.ema.filter(|r| *r > 0.0)
    }

    /// Time to move `remaining` bytes at the current rate.
    ///
    /// `None` means genuinely unknown — render that as "estimating", never as zero.
    #[must_use]
    pub fn eta(&self, remaining: u64) -> Option<Duration> {
        if remaining == 0 {
            return Some(Duration::ZERO);
        }
        let secs = remaining as f64 / self.rate()?;
        if !secs.is_finite() || secs < 0.0 {
            return None;
        }
        // Clamp the seconds, not the resulting Duration: `from_secs_f64` panics on
        // overflow, so it must never see the unclamped value in the first place.
        Some(Duration::from_secs_f64(secs.min(MAX_ETA.as_secs_f64())))
    }
}

/// A progress snapshot for display.
#[derive(Debug, Clone, PartialEq)]
pub struct FillProgress {
    /// Bytes of the job moved so far.
    pub done: u64,
    /// Bytes the job set out to move.
    pub total: u64,
    /// Smoothed bytes/sec, if known yet.
    pub rate: Option<f64>,
    /// Time remaining, if known yet.
    pub eta: Option<Duration>,
}

impl FillProgress {
    /// Completion as 0.0–1.0.
    ///
    /// Clamped, and deliberately so. `done` is derived from the device's own free-space
    /// reading, which moves for reasons that have nothing to do with us — the Kindle
    /// indexes, writes logs, rotates caches. That can push the figure past `total` or
    /// backwards below zero, and a bar that overshoots or reports `NaN` reads as a
    /// broken tool even when the fill is going fine.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }

    #[must_use]
    pub fn percent(&self) -> f64 {
        self.fraction() * 100.0
    }

    /// Bytes still to move, floored at zero.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.total.saturating_sub(self.done)
    }
}

/// Render a duration the way someone waiting on a transfer wants to read it.
#[must_use]
pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_is_unknown_before_any_sample() {
        assert_eq!(RateEstimator::new().rate(), None);
        assert_eq!(RateEstimator::new().eta(1000), None);
    }

    #[test]
    fn first_sample_is_taken_at_face_value() {
        let mut r = RateEstimator::new();
        r.observe(10_000_000, Duration::from_secs(1));
        assert_eq!(r.rate(), Some(10_000_000.0));
    }

    #[test]
    fn smoothing_moves_toward_new_samples_without_jumping_to_them() {
        let mut r = RateEstimator::with_alpha(0.5);
        r.observe(10_000_000, Duration::from_secs(1));
        r.observe(20_000_000, Duration::from_secs(1));
        assert_eq!(r.rate(), Some(15_000_000.0));
    }

    /// A zero-duration sample would divide by zero; a zero-byte sample is the ordinary
    /// pause between objects and must not drag the estimate to nothing.
    #[test]
    fn ignores_samples_that_carry_no_information() {
        let mut r = RateEstimator::new();
        r.observe(10_000_000, Duration::from_secs(1));
        let baseline = r.rate();

        r.observe(5_000_000, Duration::ZERO);
        r.observe(0, Duration::from_secs(5));
        assert_eq!(r.rate(), baseline, "estimate should be untouched");
    }

    #[test]
    fn eta_is_zero_when_nothing_is_left_even_without_a_rate() {
        assert_eq!(RateEstimator::new().eta(0), Some(Duration::ZERO));
    }

    #[test]
    fn eta_divides_remaining_by_rate() {
        let mut r = RateEstimator::new();
        r.observe(1_000_000, Duration::from_secs(1));
        assert_eq!(r.eta(10_000_000), Some(Duration::from_secs(10)));
    }

    /// A near-stalled transfer must not produce an absurd duration or panic
    /// `Duration::from_secs_f64` by overflowing it.
    #[test]
    fn eta_is_capped_rather_than_astronomical() {
        let mut r = RateEstimator::new();
        r.observe(1, Duration::from_secs(3600));
        let eta = r.eta(u64::MAX).expect("should still produce a value");
        assert_eq!(eta, MAX_ETA);
    }

    #[test]
    fn fraction_never_leaves_the_unit_interval() {
        let p = |done, total| FillProgress { done, total, rate: None, eta: None };

        assert_eq!(p(0, 100).fraction(), 0.0);
        assert_eq!(p(50, 100).fraction(), 0.5);
        assert_eq!(p(100, 100).fraction(), 1.0);
        // The device freed space behind our back: overshoot must clamp, not exceed.
        assert_eq!(p(150, 100).fraction(), 1.0);
        // Nothing to do: 0/0 must be 1.0, not NaN.
        assert_eq!(p(0, 0).fraction(), 1.0);
        assert!(p(0, 0).percent().is_finite());
    }

    #[test]
    fn remaining_floors_at_zero_when_we_overshoot() {
        let p = FillProgress { done: 150, total: 100, rate: None, eta: None };
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn durations_read_the_way_people_say_them() {
        assert_eq!(human_duration(Duration::from_secs(0)), "0s");
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(human_duration(Duration::from_secs(7_325)), "2h 02m");
    }
}
