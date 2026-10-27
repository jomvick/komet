//! Window-level frame budget measurement.
//!
//! `KOMET_WINDOW_STATS=1` records, once per root render (which runs exactly once
//! per frame), the interval between frame starts and the time spent building
//! the element tree, then logs p50/p95/p99 plus the missed-frame count at
//! `warn` level. Off by default and free when off: the env lookup is a
//! `OnceLock`, and no clock is read unless it is enabled.
//!
//! This complements the per-row cost already logged by `transcript`'s own
//! `KOMET_FRAME_STATS` knob: that one measures one transcript row's render,
//! this one measures the whole window. The two knobs are deliberately
//! distinct — a window capture must not silently switch on per-row logging,
//! which would add its own work and its own log lines to the very frames being
//! measured.
//!
//! Idle gaps are not frame drops. Between animations the app simply does not
//! render, so an interval several refresh periods long means "nothing was
//! scheduled", not "a frame was late". Such intervals are therefore excluded
//! from the samples and from the missed count; only a burst of consecutive
//! frames (`interval <= ACTIVE_GAP`) is scored against the budget. Measure
//! with an animation running — hover, scroll, or a streaming reply.
//!
//! A window closes at `window` samples, or early — straight after an
//! idle gap, once `MIN_FLUSH` samples have piled up — so a short capture
//! still yields a table instead of `no windows captured`.
//!
//! What is *not* measured: layout, paint and GPU time. The `build_*` fields
//! are only the cost of constructing the element tree inside `render`; gpui
//! does not hand the later phases to the app, so a frame can sit inside its
//! build budget and still miss the deadline while painting.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// One frame at 60 Hz. The budget a frame is scored against.
pub const FRAME_BUDGET: Duration = Duration::from_micros(16_667);

/// Intervals longer than this are treated as idle gaps, not late frames.
const ACTIVE_GAP_US: u64 = 4 * 16_667;

/// Samples required before an idle gap may close a window early. Low enough
/// that a short capture still produces a table, high enough for the
/// percentiles of that table to mean something.
const MIN_FLUSH: usize = 32;

/// Samples per logged window, matching the transcript's knob.
const WINDOW: usize = 240;

/// `KOMET_WINDOW_STATS=1` enables the measurement. Zero cost when off.
fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KOMET_WINDOW_STATS").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// One logged window of samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Active frame intervals scored in this window.
    pub frames: usize,
    /// Scored intervals longer than [`FRAME_BUDGET`].
    pub missed: usize,
    pub interval_p50_us: u64,
    pub interval_p95_us: u64,
    pub interval_p99_us: u64,
    pub interval_max_us: u64,
    /// Root render body time (element-tree construction), same window.
    /// Layout and paint are not included — see the module docs.
    pub build_p50_us: u64,
    pub build_p95_us: u64,
    pub build_p99_us: u64,
    pub build_max_us: u64,
}

/// Rolling collector. Split from the clock so the arithmetic is unit-tested.
#[derive(Debug)]
pub struct FrameBudget {
    intervals: Vec<u64>,
    builds: Vec<u64>,
    missed: usize,
    window: usize,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new(WINDOW)
    }
}

impl FrameBudget {
    pub fn new(window: usize) -> Self {
        Self {
            intervals: Vec::with_capacity(window),
            builds: Vec::with_capacity(window),
            missed: 0,
            window: window.max(1),
        }
    }

    /// Record one frame. `interval_us` is `None` for the first frame of a run
    /// or after an idle gap. Returns a [`Summary`] once the window holds
    /// `window` samples — or `MIN_FLUSH` samples immediately after an
    /// idle gap, since a burst has just ended and waiting for the full window
    /// could mean never emitting at all. The samples are cleared either way.
    pub fn record(&mut self, interval_us: Option<u64>, build_us: u64) -> Option<Summary> {
        let scored = interval_us.filter(|us| *us > 0 && *us <= ACTIVE_GAP_US);
        // Nothing was scheduled before this frame: whatever piled up since the
        // last one is a complete burst, not a partial sample.
        let burst_ended = interval_us.is_some_and(|us| us > ACTIVE_GAP_US);
        if let Some(us) = scored {
            if us > FRAME_BUDGET.as_micros() as u64 {
                self.missed += 1;
            }
            self.intervals.push(us);
        }
        self.builds.push(build_us);
        // The window is one frame long per build sample; idle gaps contribute
        // a build but no interval, so the frame count can trail it.
        if self.builds.len() < self.window && !(burst_ended && self.builds.len() >= MIN_FLUSH) {
            return None;
        }
        let mut intervals = std::mem::take(&mut self.intervals);
        let mut builds = std::mem::take(&mut self.builds);
        intervals.sort_unstable();
        builds.sort_unstable();
        let summary = Summary {
            frames: intervals.len(),
            missed: self.missed,
            interval_p50_us: percentile(&intervals, 50),
            interval_p95_us: percentile(&intervals, 95),
            interval_p99_us: percentile(&intervals, 99),
            interval_max_us: *intervals.last().unwrap_or(&0),
            build_p50_us: percentile(&builds, 50),
            build_p95_us: percentile(&builds, 95),
            build_p99_us: percentile(&builds, 99),
            build_max_us: *builds.last().unwrap_or(&0),
        };
        self.intervals = Vec::with_capacity(self.window);
        self.builds = Vec::with_capacity(self.window);
        self.missed = 0;
        Some(summary)
    }
}

/// Nearest-rank percentile over an ascending slice.
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let ix = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[ix]
}

thread_local! {
    static BUDGET: RefCell<FrameBudget> = RefCell::new(FrameBudget::default());
    static LAST_START: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn record(interval_us: Option<u64>, build: Duration) {
    let summary = BUDGET.with(|b| b.borrow_mut().record(interval_us, build.as_micros() as u64));
    if let Some(s) = summary {
        tracing::warn!(
            frames = s.frames,
            missed = s.missed,
            interval_p50_us = s.interval_p50_us,
            interval_p95_us = s.interval_p95_us,
            interval_p99_us = s.interval_p99_us,
            interval_max_us = s.interval_max_us,
            build_p50_us = s.build_p50_us,
            build_p95_us = s.build_p95_us,
            build_p99_us = s.build_p99_us,
            build_max_us = s.build_max_us,
            "window frame budget"
        );
    }
}

/// Time one root render pass. Returns `None` when the knob is off.
///
/// Bind it to a name that lives to the end of `render` (`let _frame = ...`):
/// the drop is what records the build time.
pub struct FrameGuard {
    start: Instant,
    interval_us: Option<u64>,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        record(self.interval_us, self.start.elapsed());
    }
}

/// Start a measurement for this frame, if enabled.
pub fn frame() -> Option<FrameGuard> {
    if !enabled() {
        return None;
    }
    let now = Instant::now();
    let interval_us = LAST_START.with(|last| {
        let prev = last.replace(Some(now));
        prev.map(|p| now.saturating_duration_since(p).as_micros() as u64)
    });
    Some(FrameGuard {
        start: now,
        interval_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank_and_never_overruns() {
        let sorted = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&sorted, 50), 30);
        assert_eq!(percentile(&sorted, 99), 50);
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 99), 7);
    }

    #[test]
    fn idle_gaps_are_not_late_frames() {
        let mut budget = FrameBudget::new(3);
        // 200ms gap: the app was idle, so it is neither sampled nor missed.
        assert!(budget.record(Some(200_000), 1_000).is_none());
        // Two frames inside the burst, one over budget.
        budget.record(Some(8_000), 1_000);
        let summary = budget.record(Some(30_000), 1_000).expect("window full");
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.missed, 1);
        assert_eq!(summary.interval_max_us, 30_000);
    }

    #[test]
    fn first_frame_is_not_scored_and_clears_after_a_window() {
        let mut budget = FrameBudget::new(2);
        assert!(budget.record(None, 900).is_none());
        let summary = budget.record(Some(16_000), 700).expect("window full");
        assert_eq!(summary.frames, 1);
        assert_eq!(summary.missed, 0);
        // Nearest rank: two build samples, so p50 lands on the upper one.
        assert_eq!(summary.build_p50_us, 900);
        assert_eq!(summary.build_max_us, 900);
        // The window resets: the next full one starts from zero.
        assert!(budget.record(Some(16_000), 500).is_none());
        let next = budget.record(Some(16_000), 500).expect("second window");
        assert_eq!(next.frames, 2);
        assert_eq!(next.missed, 0);
    }

    #[test]
    fn an_idle_gap_closes_a_partial_window() {
        let mut budget = FrameBudget::new(240);
        // A burst too short to fill the window: nothing is emitted yet.
        for _ in 0..40 {
            assert!(budget.record(Some(8_000), 1_000).is_none());
        }
        // The pause ends the burst, so the partial window is emitted instead
        // of being lost. The pause itself is not one of the scored frames.
        let summary = budget.record(Some(500_000), 1_000).expect("burst closed");
        assert_eq!(summary.frames, 40);
        assert_eq!(summary.missed, 0);
        // And the samples are cleared for the next burst.
        assert!(budget.record(Some(8_000), 1_000).is_none());
    }

    #[test]
    fn build_percentiles_follow_the_samples() {
        let mut budget = FrameBudget::new(4);
        let mut summary = None;
        for build in [100, 400, 200, 300] {
            summary = budget.record(Some(16_000), build);
        }
        let summary = summary.expect("window full");
        assert_eq!(summary.frames, 4);
        assert_eq!(summary.build_p50_us, 300);
        assert_eq!(summary.build_p99_us, 400);
        assert_eq!(summary.build_max_us, 400);
    }
}
