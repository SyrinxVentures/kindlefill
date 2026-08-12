//! Pure convergence logic: given the free space the device reports right now,
//! how many bytes should we write next?
//!
//! Deliberately free of I/O so the hard part — landing inside a 40 MB window on a
//! 16 GB device, a 0.25% target — can be tested without a Kindle attached.
//!
//! The loop is measure-write-remeasure, never arithmetic bookkeeping. Every file
//! costs slightly more than its own bytes (directory entries, block rounding), and
//! that overhead is unknowable from the host side. Re-reading free space after each
//! write absorbs it for free; predicting it does not.

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// Write sizes we're willing to use, largest first.
///
/// Bulk rungs are large to amortize MTP's per-object cost: every object is a
/// `SendObjectInfo` + `SendObject` round trip plus a device-side commit, so 13
/// objects of 1 GiB beat 130 of 100 MiB. The small rungs exist only for the final
/// approach, where precision matters more than throughput.
///
/// Nothing here comes near 4 GiB — classic MTP `ObjectInfo` carries a 32-bit size
/// field, so a single object must stay under `u32::MAX` bytes.
const LADDER: &[u64] = &[
    GIB,
    256 * MIB,
    64 * MIB,
    16 * MIB,
    4 * MIB,
    MIB,
    256 * KIB,
    64 * KIB,
    16 * KIB,
    4 * KIB,
];

/// The free-space window we're trying to land in, e.g. 50-90 MB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    low: u64,
    high: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowError {
    /// `low` was not below `high`.
    Inverted { low: u64, high: u64 },
    /// The window was too narrow to land in reliably.
    TooNarrow { width: u64, minimum: u64 },
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowError::Inverted { low, high } => {
                write!(f, "lower bound {low} must be below upper bound {high}")
            }
            WindowError::TooNarrow { width, minimum } => write!(
                f,
                "window is {width} bytes wide; needs at least {minimum} to absorb \
                 filesystem overhead on the final write"
            ),
        }
    }
}

impl std::error::Error for WindowError {}

impl Window {
    /// Narrower than this and the last write's own metadata overhead could carry us
    /// straight through the window and out the bottom.
    pub const MIN_WIDTH: u64 = 4 * MIB;

    pub fn new(low: u64, high: u64) -> Result<Self, WindowError> {
        if low >= high {
            return Err(WindowError::Inverted { low, high });
        }
        let width = high - low;
        if width < Self::MIN_WIDTH {
            return Err(WindowError::TooNarrow {
                width,
                minimum: Self::MIN_WIDTH,
            });
        }
        Ok(Window { low, high })
    }

    pub fn low(&self) -> u64 {
        self.low
    }

    pub fn high(&self) -> u64 {
        self.high
    }

    /// Where we actually steer: the middle, not an edge.
    ///
    /// The Kindle keeps writing after we disconnect — indexes, thumbnails, logs —
    /// so parking at `low` risks drifting under it, and parking at `high` leaves no
    /// margin if our own final write costs a little more than we asked for.
    pub fn aim(&self) -> u64 {
        self.low + (self.high - self.low) / 2
    }

    pub fn contains(&self, free: u64) -> bool {
        free >= self.low && free <= self.high
    }
}

/// What to do next, given the free space just read back from the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Upload one object of this many bytes, then re-read free space.
    Write(u64),
    /// Free space is inside the window. Stop.
    Done,
    /// Free space is already below the window — we (or the device) overshot.
    /// Recoverable by deleting filler, which is why it isn't an error.
    Overfilled { free: u64, excess: u64 },
}

/// Decide the next move from the currently reported free space.
///
/// Call this after every single write, against a *freshly refreshed* reading.
/// Feeding it a stale number is the one way to make it misbehave: MTP caches
/// `StorageInfo`, so the value must come from a new `GetStorageInfo`, not from the
/// struct captured when the storage was opened.
pub fn next_step(free: u64, window: Window) -> Step {
    if free < window.low() {
        return Step::Overfilled {
            free,
            excess: window.low() - free,
        };
    }
    if free <= window.high() {
        return Step::Done;
    }

    // Still above the window. Steer at the midpoint rather than the upper edge so a
    // slight overshoot lands inside instead of just past it.
    let deficit = free - window.aim();

    // Hard ceiling: writing this much would put us exactly at `low`. The ladder pick
    // below is always smaller (aim > low), so this never binds in practice — it's here
    // so a future ladder change can't silently start overshooting.
    let max_safe = free - window.low();

    let size = LADDER
        .iter()
        .copied()
        .find(|&rung| rung <= deficit)
        // Closer than the smallest rung: ask for exactly what's left.
        .unwrap_or(deficit);

    Step::Write(size.min(max_safe).max(1))
}

/// Which filler object to give back when free space is *below* the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Delete the object at this index of the slice passed in.
    Remove(usize),
    /// Below the window with nothing of ours left to remove. Not recoverable by this
    /// tool — the space is someone else's.
    Exhausted,
}

/// Pick the filler object to delete to climb back toward the window.
///
/// The counterpart to [`next_step`], and the reason a fill can converge from either
/// side. Writing can only ever reduce free space, so a device that ends up *under* the
/// target — because the window moved, or the Kindle grew its own files — was
/// previously stuck: the only remedy was deleting all the filler and starting over,
/// seventeen minutes to gain a few megabytes.
///
/// Preference order, and the middle one is the point:
///
/// 1. An object that lands us *inside* the window outright — no rewriting needed.
/// 2. Otherwise the **smallest** object that reaches `low`. It overshoots past `high`,
///    and the ladder then writes back down. That indirection is unavoidable: filler is
///    written in coarse rungs, so on a real device the smallest object may be 64 MiB
///    while the window is 20 MB wide. Deleting is never enough on its own.
/// 3. Otherwise the largest available, which at least makes progress.
///
/// Termination rests on [`next_step`] never proposing a write that lands below `low`.
/// Once a removal carries us to or above `low`, writes converge downward and cannot
/// re-enter this branch, so removals can't alternate with writes. Each removal also
/// strictly increases free space, and the filler set is finite.
#[must_use]
pub fn next_removal(free: u64, window: Window, sizes: &[u64]) -> Removal {
    if sizes.is_empty() {
        return Removal::Exhausted;
    }
    // 1. Lands inside the window: prefer whichever ends up nearest the aim.
    let inside = sizes
        .iter()
        .enumerate()
        .filter(|(_, &s)| window.contains(free.saturating_add(s)))
        .min_by_key(|(_, &s)| free.saturating_add(s).abs_diff(window.aim()));
    if let Some((i, _)) = inside {
        return Removal::Remove(i);
    }
    // 2. Smallest that at least reaches the window, accepting the overshoot.
    let reaches = sizes
        .iter()
        .enumerate()
        .filter(|(_, &s)| free.saturating_add(s) >= window.low())
        .min_by_key(|(_, &s)| s);
    if let Some((i, _)) = reaches {
        return Removal::Remove(i);
    }
    // 3. Nothing reaches it; take the biggest bite available.
    let (i, _) = sizes
        .iter()
        .enumerate()
        .max_by_key(|(_, &s)| s)
        .expect("non-empty");
    Removal::Remove(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Window {
        Window::new(50 * MIB, 90 * MIB).unwrap()
    }

    #[test]
    fn rejects_inverted_and_hairline_windows() {
        assert!(matches!(
            Window::new(90 * MIB, 50 * MIB),
            Err(WindowError::Inverted { .. })
        ));
        assert!(matches!(
            Window::new(50 * MIB, 50 * MIB + 1024),
            Err(WindowError::TooNarrow { .. })
        ));
        assert!(Window::new(50 * MIB, 90 * MIB).is_ok());
    }

    #[test]
    fn aims_at_the_middle_not_an_edge() {
        assert_eq!(window().aim(), 70 * MIB);
    }

    #[test]
    fn done_only_inside_the_window() {
        let w = window();
        assert_eq!(next_step(70 * MIB, w), Step::Done);
        assert_eq!(next_step(50 * MIB, w), Step::Done, "low bound is inclusive");
        assert_eq!(
            next_step(90 * MIB, w),
            Step::Done,
            "high bound is inclusive"
        );
    }

    #[test]
    fn reports_overfill_below_the_window() {
        match next_step(40 * MIB, window()) {
            Step::Overfilled { excess, .. } => assert_eq!(excess, 10 * MIB),
            other => panic!("expected Overfilled, got {other:?}"),
        }
    }

    #[test]
    fn uses_big_rungs_when_far_away_and_small_ones_when_close() {
        let w = window();
        assert_eq!(next_step(13 * GIB, w), Step::Write(GIB));

        // Just past the upper edge. Because we steer at the midpoint, the smallest
        // deficit this window can ever present is half its width, so the final
        // approach here is a 16 MiB rung — not a tiny one.
        assert_eq!(next_step(90 * MIB + 1, w), Step::Write(16 * MIB));

        // A minimum-width window is what actually forces the small rungs into play.
        let narrow = Window::new(50 * MIB, 54 * MIB).unwrap();
        assert_eq!(narrow.aim(), 52 * MIB);
        assert_eq!(next_step(54 * MIB + 1, narrow), Step::Write(MIB));
    }

    /// Documents why the `unwrap_or(deficit)` fallback in `next_step` is defensive
    /// rather than load-bearing: reaching it needs a deficit below the smallest rung,
    /// and the smallest deficit any legal window can present is `MIN_WIDTH / 2`.
    #[test]
    fn smallest_possible_deficit_still_lands_on_a_ladder_rung() {
        let narrowest = Window::new(50 * MIB, 50 * MIB + Window::MIN_WIDTH).unwrap();
        let smallest_deficit = narrowest.high() - narrowest.aim();
        assert!(smallest_deficit >= *LADDER.last().unwrap());
    }

    #[test]
    fn never_proposes_a_write_that_would_undershoot_the_window() {
        let w = window();
        // Sweep the whole approach and assert the invariant that actually matters:
        // no single proposed write can take free space below `low`.
        let mut free = 40 * GIB;
        while free > w.high() {
            match next_step(free, w) {
                Step::Write(n) => {
                    assert!(n > 0, "must make progress at free={free}");
                    assert!(
                        free - n >= w.low(),
                        "write of {n} at free={free} would undershoot {}",
                        w.low()
                    );
                    free -= n;
                }
                other => panic!("expected Write while above window, got {other:?}"),
            }
        }
        assert!(w.contains(free));
    }

    /// The realistic case: every object costs more than its own bytes. If the loop
    /// only worked with zero overhead it would be useless on a real filesystem.
    #[test]
    fn converges_despite_per_file_overhead() {
        let w = window();
        for overhead in [0, 512, 4 * KIB, 64 * KIB] {
            let mut free = 13 * GIB;
            let mut writes = 0;
            loop {
                match next_step(free, w) {
                    Step::Done => break,
                    Step::Overfilled { free, .. } => {
                        panic!("overhead={overhead}: overshot to {free}")
                    }
                    Step::Write(n) => {
                        // Model the device: the object costs its size plus metadata,
                        // but can never drive free space below zero.
                        free = free.saturating_sub(n + overhead);
                        writes += 1;
                        assert!(writes < 10_000, "overhead={overhead}: not converging");
                    }
                }
            }
            assert!(
                w.contains(free),
                "overhead={overhead}: landed at {free}, outside window"
            );
        }
    }

    #[test]
    fn bulk_of_a_full_fill_is_large_objects() {
        // Guards the throughput property: a 13 GiB fill must not degenerate into
        // thousands of small objects, each costing an MTP round trip.
        let w = window();
        let mut free = 13 * GIB;
        let mut count = 0;
        while let Step::Write(n) = next_step(free, w) {
            free -= n;
            count += 1;
        }
        assert!(
            count < 40,
            "took {count} objects to fill 13 GiB; too chatty"
        );
    }

    #[test]
    fn nothing_to_remove_is_a_terminal_state_not_an_index() {
        assert_eq!(next_removal(10 * MIB, window(), &[]), Removal::Exhausted);
    }

    #[test]
    fn prefers_a_removal_that_lands_inside_the_window_outright() {
        let w = window(); // 50-90 MiB, aim 70
                          // From 40 MiB free: +32 lands on 72, +45 lands on 85. Both are inside; 72 is
                          // nearer the aim, so it wins.
        let sizes = [4 * MIB, 32 * MIB, 45 * MIB, GIB];
        assert_eq!(next_removal(40 * MIB, w, &sizes), Removal::Remove(1));
    }

    /// The case a real device produced: 87 MB free, wanting ~80, and the smallest
    /// object on the device is 64 MiB. Nothing lands inside the window, so the
    /// smallest that reaches it is taken and the ladder writes back down.
    #[test]
    fn overshoots_with_the_smallest_object_when_none_lands_inside() {
        let w = Window::new(75 * MIB, 85 * MIB).unwrap();
        let sizes = [GIB, 256 * MIB, 64 * MIB, 64 * MIB];
        // 20 MiB free: every option overshoots past 85, so take a 64 MiB one.
        match next_removal(20 * MIB, w, &sizes) {
            Removal::Remove(i) => assert_eq!(sizes[i], 64 * MIB, "took {} MiB", sizes[i] / MIB),
            other => panic!("expected a removal, got {other:?}"),
        }
    }

    #[test]
    fn takes_the_largest_when_nothing_reaches_the_window() {
        let w = window();
        let sizes = [MIB, 4 * MIB, 2 * MIB];
        // 1 MiB free; even +4 MiB is far below the 50 MiB floor. Biggest bite.
        assert_eq!(next_removal(MIB, w, &sizes), Removal::Remove(1));
    }

    /// The property the bidirectional loop depends on: from anywhere below the window,
    /// alternating removals and writes reaches the window in bounded steps and never
    /// oscillates. A write can't drop us back under `low`, so the removal branch is
    /// entered only before the first write.
    #[test]
    fn converges_from_below_the_window_without_oscillating() {
        let w = Window::new(75 * MIB, 85 * MIB).unwrap();
        for start in [0, MIB, 20 * MIB, 74 * MIB] {
            let mut sizes = vec![GIB, 256 * MIB, 64 * MIB, 64 * MIB, 16 * MIB];
            let mut free = start;
            let (mut removals, mut writes) = (0, 0);
            loop {
                if w.contains(free) {
                    break;
                }
                if free < w.low() {
                    assert_eq!(writes, 0, "removal after a write means oscillation");
                    match next_removal(free, w, &sizes) {
                        Removal::Remove(i) => {
                            free += sizes.remove(i);
                            removals += 1;
                        }
                        Removal::Exhausted => panic!("ran out of filler from {start}"),
                    }
                } else {
                    match next_step(free, w) {
                        Step::Write(n) => {
                            free -= n;
                            writes += 1;
                        }
                        Step::Done => break,
                        other => panic!("unexpected {other:?}"),
                    }
                }
                assert!(removals + writes < 100, "not converging from {start}");
            }
            assert!(w.contains(free), "from {start} landed at {free}");
        }
    }

    #[test]
    fn no_single_object_can_exceed_the_mtp_32_bit_size_field() {
        for &rung in LADDER {
            assert!(
                rung < u64::from(u32::MAX),
                "rung {rung} overflows ObjectInfo"
            );
        }
    }
}
