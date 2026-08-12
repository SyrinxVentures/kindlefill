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

/// Transfer rate over a trailing time window, in bytes per second.
///
/// **Bytes moved divided by time elapsed** — not an average of instantaneous rate
/// samples. That distinction is the whole design, and getting it wrong is not merely
/// cosmetic: MTP throughput arrives in bursts as the host buffer flushes, so a 100 ms
/// sample carrying 10 MB reads as 100 MB/s. Averaging *rates* gives that burst the
/// same weight as a slow sample and lands high; on a real Paperwhite transfer running
/// at a true 25.5 MB/s, an exponential average of samples read 22-86 MB/s and centred
/// near 35. Dividing total bytes by total time cannot do that — it reported 23-27.
///
/// The window also makes the estimate independent of how often progress is reported.
/// Weighting each sample equally means the answer changes when the callback cadence
/// changes, which is not a property anyone wants from a throughput readout.
///
/// Clock-free by design: elapsed time is passed in rather than read, so every case
/// below is testable without sleeping.
#[derive(Debug, Clone)]
pub struct RateEstimator {
    /// `(elapsed_since_job_start, cumulative_bytes)`, oldest first. Trimmed to
    /// [`ETA_WINDOW`], which is the longest horizon anything here asks for.
    samples: std::collections::VecDeque<(Duration, u64)>,
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RateEstimator {
    /// Horizon for the displayed rate. Short enough to show a real slowdown promptly,
    /// long enough to span several of the device's commit pauses. Measured on hardware:
    /// 5 s held the readout inside a 3.3 MB/s band on a transfer whose true rate was
    /// 25.5, where 2 s gave 10 MB/s of swing.
    pub const RATE_WINDOW: Duration = Duration::from_secs(5);

    /// Horizon for the ETA. Longer, because a remaining-time figure that twitches is
    /// worse than one that lags — but still bounded, so a device that genuinely slows
    /// down in the last third is reflected rather than averaged away.
    pub const ETA_WINDOW: Duration = Duration::from_secs(30);

    /// Below this much history the window is too short to divide by honestly, and the
    /// answer would be the same burst artefact the window exists to avoid.
    const MIN_SPAN: Duration = Duration::from_millis(750);

    #[must_use]
    pub fn new() -> Self {
        Self {
            samples: std::collections::VecDeque::new(),
        }
    }

    /// Record that `cumulative` bytes of the job had moved at `at` (elapsed since the
    /// job began).
    ///
    /// Cumulative rather than incremental because the window needs to subtract two
    /// readings taken seconds apart, and reconstructing that from deltas would just be
    /// this buffer with extra steps.
    ///
    /// A reading that goes *backwards* clears the history instead of being dropped.
    /// It happens for real: `done` is re-derived from the device's own free-space
    /// figure at each object boundary, and per-file overhead means free space can fall
    /// further than the bytes we wrote — or rise, if the Kindle tidies up behind us.
    /// Merely ignoring the sample would leave the next good one measuring its delta
    /// against an anchor from before the step, and no clamp fixes a window that spans
    /// a discontinuity.
    pub fn observe(&mut self, cumulative: u64, at: Duration) {
        if let Some(&(last_at, last_bytes)) = self.samples.back() {
            if cumulative < last_bytes {
                self.samples.clear();
            } else if at < last_at {
                // Time cannot run backwards; a caller that says so is confused enough
                // that the safe move is to start over rather than reason about it.
                self.samples.clear();
            }
        }
        self.samples.push_back((at, cumulative));
        while let Some(&(oldest, _)) = self.samples.front() {
            if at.saturating_sub(oldest) > Self::ETA_WINDOW && self.samples.len() > 2 {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Throughput over `window`, or `None` until there is enough history to divide by.
    fn windowed(&self, window: Duration) -> Option<f64> {
        let &(newest_at, newest_bytes) = self.samples.back()?;
        // Oldest sample still inside the window; falls back to the oldest we have.
        let cutoff = newest_at.saturating_sub(window);
        let (from_at, from_bytes) = self
            .samples
            .iter()
            .find(|(at, _)| *at >= cutoff)
            .copied()
            .or_else(|| self.samples.front().copied())?;

        let secs = newest_at.saturating_sub(from_at).as_secs_f64();
        if secs < Self::MIN_SPAN.as_secs_f64() {
            return None;
        }
        let bytes = newest_bytes.saturating_sub(from_bytes) as f64;
        let rate = bytes / secs;
        // Zero is a legitimate answer, not a missing one. A window in which nothing
        // moved means the transfer has stalled, and "0.0 MB/s" says that; "—" claims
        // we don't know, which is a different and less useful statement. `eta` divides
        // by this and guards the resulting infinity itself.
        rate.is_finite().then_some(rate)
    }

    /// Bytes per second over the display window, or `None` before it means anything.
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        self.windowed(Self::RATE_WINDOW)
    }

    /// Time to move `remaining` bytes, from a longer horizon than [`rate`].
    ///
    /// `None` means genuinely unknown — render that as "estimating", never as zero.
    #[must_use]
    pub fn eta(&self, remaining: u64) -> Option<Duration> {
        if remaining == 0 {
            return Some(Duration::ZERO);
        }
        let secs = remaining as f64 / self.windowed(Self::ETA_WINDOW)?;
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

/// Render a *remaining* time, rounded to a precision anyone would believe.
///
/// Separate from [`human_duration`], which stays exact, because the two are answering
/// different questions. An estimate printed to the second invites you to watch the
/// last digit, and the last digit is the part that is least true — a 17-minute
/// transfer does not know its own finish to within a second. Rounding coarser as the
/// figure grows keeps the display still while the underlying estimate moves within
/// its own error, which is the point: the numbers stop twitching without anything
/// being smoothed away.
#[must_use]
pub fn human_eta(d: Duration) -> String {
    let secs = d.as_secs();
    let step = match secs {
        0..=59 => 5,
        60..=599 => 10,
        _ => 60,
    };
    // Round to the nearest step, but never down to "0s" while work remains — that
    // reads as finished.
    let rounded = ((secs + step / 2) / step * step).max(if secs > 0 { step } else { 0 });
    human_duration(Duration::from_secs(rounded))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a steady `bytes_per_sec` for `secs`, sampling `per_sec` times a second,
    /// continuing from `(at, bytes)`. Returns where it left off, so segments at
    /// different speeds can be chained without the byte count ever stepping back.
    fn steady(
        r: &mut RateEstimator,
        bytes_per_sec: u64,
        secs: f64,
        per_sec: f64,
        at: Duration,
        bytes: u64,
    ) -> (Duration, u64) {
        let n = (secs * per_sec).round() as u64;
        let (mut t, mut total) = (at, bytes);
        for i in 1..=n {
            let offset = i as f64 / per_sec;
            t = at + Duration::from_secs_f64(offset);
            total = bytes + (bytes_per_sec as f64 * offset) as u64;
            r.observe(total, t);
        }
        (t, total)
    }

    #[test]
    fn rate_is_unknown_before_there_is_enough_history_to_divide_by() {
        let mut r = RateEstimator::new();
        assert_eq!(r.rate(), None);
        assert_eq!(r.eta(1000), None);
        // A single reading spans no time at all.
        r.observe(10_000_000, Duration::from_millis(100));
        assert_eq!(r.rate(), None, "one sample is not a rate");
        // Still inside MIN_SPAN.
        r.observe(20_000_000, Duration::from_millis(400));
        assert_eq!(r.rate(), None);
    }

    #[test]
    fn rate_is_bytes_over_elapsed_time() {
        let mut r = RateEstimator::new();
        steady(&mut r, 10_000_000, 4.0, 10.0, Duration::ZERO, 0);
        let rate = r.rate().expect("rate");
        assert!(
            (rate - 10_000_000.0).abs() < 100_000.0,
            "expected ~10 MB/s, got {rate}"
        );
    }

    /// The bug this design exists to prevent. MTP delivers bursts: the host buffer
    /// flushes, then the device commits while nothing moves. Averaging *instantaneous
    /// rates* weights the 100 ms burst the same as the 900 ms lull and lands roughly
    /// 10x high. Dividing bytes by elapsed time cannot.
    #[test]
    fn bursty_transfer_reports_its_true_average_not_its_peaks() {
        let mut r = RateEstimator::new();
        let (mut t, mut total) = (Duration::ZERO, 0u64);
        // 1 MB every second, but delivered in a single 100 ms burst each time:
        // instantaneous peak is 10 MB/s, true throughput is 1 MB/s.
        for _ in 0..30 {
            t += Duration::from_millis(100);
            total += 1_000_000;
            r.observe(total, t);
            t += Duration::from_millis(900);
            r.observe(total, t); // the lull: no new bytes
        }
        let rate = r.rate().expect("rate");
        assert!(
            (rate - 1_000_000.0).abs() < 100_000.0,
            "true rate is 1 MB/s; reported {rate} — peaks are leaking into the average"
        );
    }

    /// The same bytes over the same time must read the same regardless of how often
    /// progress happened to be reported. Weighting per sample got this wrong.
    #[test]
    fn reported_rate_does_not_depend_on_callback_cadence() {
        let (mut sparse, mut dense) = (RateEstimator::new(), RateEstimator::new());
        steady(&mut sparse, 8_000_000, 10.0, 1.0, Duration::ZERO, 0);
        steady(&mut dense, 8_000_000, 10.0, 20.0, Duration::ZERO, 0);
        let (a, b) = (sparse.rate().unwrap(), dense.rate().unwrap());
        assert!(
            (a - b).abs() / a < 0.05,
            "cadence changed the answer: {a} vs {b}"
        );
    }

    /// A pause between objects is not a slowdown to zero, but it is real elapsed time
    /// and the rate should reflect it rather than ignoring it.
    #[test]
    fn a_stall_pulls_the_rate_down_rather_than_being_ignored() {
        let mut r = RateEstimator::new();
        let (_, done) = steady(&mut r, 10_000_000, 5.0, 10.0, Duration::ZERO, 0);
        let moving = r.rate().unwrap();
        // Four seconds where nothing moves.
        r.observe(done, Duration::from_secs(9));
        let stalled = r.rate().unwrap();
        assert!(
            stalled < moving * 0.5,
            "{stalled} should be well under {moving}"
        );
    }

    /// `done` is re-derived from the device's free space each object, so it can step
    /// backwards. The window must not span the discontinuity.
    #[test]
    fn a_backwards_reading_restarts_the_window_instead_of_corrupting_it() {
        let mut r = RateEstimator::new();
        steady(&mut r, 10_000_000, 5.0, 10.0, Duration::ZERO, 0);
        assert!(r.rate().is_some());

        // The device reports less done than we already saw.
        r.observe(1_000_000, Duration::from_secs(6));
        assert_eq!(
            r.rate(),
            None,
            "history should have been dropped, not blended"
        );

        steady(
            &mut r,
            4_000_000,
            4.0,
            10.0,
            Duration::from_secs(6),
            1_000_000,
        );
        let rate = r.rate().expect("should recover");
        assert!(
            (rate - 4_000_000.0).abs() < 400_000.0,
            "expected ~4 MB/s after re-anchoring, got {rate}"
        );
    }

    #[test]
    fn eta_is_zero_when_nothing_is_left_even_without_a_rate() {
        assert_eq!(RateEstimator::new().eta(0), Some(Duration::ZERO));
    }

    #[test]
    fn eta_divides_remaining_by_rate() {
        let mut r = RateEstimator::new();
        steady(&mut r, 1_000_000, 4.0, 10.0, Duration::ZERO, 0);
        let eta = r.eta(10_000_000).expect("eta");
        assert!(
            (eta.as_secs_f64() - 10.0).abs() < 0.5,
            "expected ~10s, got {eta:?}"
        );
    }

    /// A near-stalled transfer must not produce an absurd duration or panic
    /// `Duration::from_secs_f64` by overflowing it.
    #[test]
    fn eta_is_capped_rather_than_astronomical() {
        let mut r = RateEstimator::new();
        r.observe(0, Duration::ZERO);
        r.observe(1, Duration::from_secs(20));
        let eta = r.eta(u64::MAX).expect("should still produce a value");
        assert_eq!(eta, MAX_ETA);
    }

    /// The ETA leans on a longer horizon than the displayed rate, so a brief dip
    /// moves the remaining-time figure less than it moves MB/s.
    #[test]
    fn eta_uses_a_longer_horizon_than_the_displayed_rate() {
        assert!(RateEstimator::ETA_WINDOW > RateEstimator::RATE_WINDOW);
        let mut r = RateEstimator::new();
        let (at, done) = steady(&mut r, 10_000_000, 25.0, 10.0, Duration::ZERO, 0);
        let (rate_before, eta_before) = (r.rate().unwrap(), r.eta(100_000_000).unwrap());

        // Five seconds at a tenth of the speed: fills the rate window completely, but
        // is only a sixth of the ETA window.
        steady(&mut r, 1_000_000, 5.0, 10.0, at, done);
        let (rate_after, eta_after) = (r.rate().unwrap(), r.eta(100_000_000).unwrap());

        let rate_change = (rate_before - rate_after) / rate_before;
        let eta_change =
            (eta_after.as_secs_f64() - eta_before.as_secs_f64()) / eta_before.as_secs_f64();
        assert!(
            rate_change > eta_change,
            "the displayed rate should react more sharply than the ETA \
             (rate fell {:.1}, eta rose {:.1})",
            rate_change,
            eta_change
        );
    }

    #[test]
    fn fraction_never_leaves_the_unit_interval() {
        let p = |done, total| FillProgress {
            done,
            total,
            rate: None,
            eta: None,
        };

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
        let p = FillProgress {
            done: 150,
            total: 100,
            rate: None,
            eta: None,
        };
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn durations_read_the_way_people_say_them() {
        assert_eq!(human_duration(Duration::from_secs(0)), "0s");
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(human_duration(Duration::from_secs(7_325)), "2h 02m");
    }

    #[test]
    fn eta_is_rounded_coarser_the_further_out_it_is() {
        let s = |n| human_eta(Duration::from_secs(n));
        assert_eq!(s(0), "0s");
        assert_eq!(s(43), "45s", "under a minute rounds to 5s");
        assert_eq!(s(127), "2m 10s", "under ten minutes rounds to 10s");
        assert_eq!(
            s(1_016),
            "17m 00s",
            "beyond ten minutes rounds to the minute"
        );
        assert_eq!(s(7_325), "2h 02m");
    }

    /// Rounding must never render remaining work as "0s"; that reads as finished.
    #[test]
    fn a_nearly_finished_eta_never_rounds_down_to_nothing() {
        assert_eq!(human_eta(Duration::from_secs(1)), "5s");
        assert_eq!(human_eta(Duration::from_secs(2)), "5s");
        assert_eq!(
            human_eta(Duration::ZERO),
            "0s",
            "only genuine zero shows zero"
        );
    }

    /// The display should hold still while the underlying estimate wobbles inside its
    /// own error — that is what stops the last digit twitching every 100 ms.
    #[test]
    fn small_wobbles_do_not_change_the_rendered_eta() {
        // 16m 40s through 16m 52s all round to the same minute.
        for secs in [1_000, 1_004, 1_008, 1_012] {
            assert_eq!(human_eta(Duration::from_secs(secs)), "17m 00s", "{secs}");
        }
    }
}
