//! Window-level frame budget measurement.
//!
//! `KOMET_WINDOW_STATS=1` records the interval between frame starts, the time
//! spent building the element tree, and the time child views spend rendering,
//! then logs p50/p95/p99 plus the missed-frame count at `warn` level. Off by
//! default and free when off: the env lookup is a `OnceLock`, and no clock is
//! read unless it is enabled.
//!
//! Child renders happen *after* the root render returns: gpui builds a
//! `ViewElement` for each embedded entity and only renders it during that
//! element's prepaint, inside `draw_roots`. A sample is therefore finalized at
//! the start of the *next* frame, once both the root body (`build_*`) and the
//! instrumented children (`children_*`) are complete. `total_*` is the sum of
//! the two per frame, so "Shell alone" and "children" are comparable.
//!
//! `transcript_renders`, `composer_renders` and `changes_renders` count child
//! renders per window. With gpui view caching enabled, a burst that only
//! animates a Shell-owned element must show zero renders for the models.
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
//! An *inactive* window is not scored either: gpui throttles background
//! windows to ~30 fps, so every interval sits near 33 ms and would count as
//! missed even though nothing is late. Capture with the window focused.
//!
//! A window closes at `window` samples, or early — straight after an
//! idle gap, once `MIN_FLUSH` samples have piled up — so a short capture
//! still yields a table instead of `no windows captured`.
//!
//! What is *not* measured: layout, paint and GPU time. gpui does not hand the
//! later phases to the app, so a frame can sit inside its build budget and
//! still miss the deadline while painting. Cached child views do no layout or
//! paint work of their own — that shows up as fewer child renders, not as a
//! shorter `children_*`.

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

/// Child views whose render cost is attributed to the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildView {
    Transcript = 0,
    Composer = 1,
    Changes = 2,
}

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
    /// Root render body (element-tree construction).
    pub build_p50_us: u64,
    pub build_p95_us: u64,
    pub build_p99_us: u64,
    pub build_max_us: u64,
    /// Sum of the instrumented child render bodies, same frames.
    pub children_p50_us: u64,
    pub children_p95_us: u64,
    pub children_p99_us: u64,
    pub children_max_us: u64,
    /// `build + children` per frame.
    pub total_p50_us: u64,
    pub total_p95_us: u64,
    pub total_p99_us: u64,
    pub total_max_us: u64,
    /// Child renders in this window — zero for a view left cached.
    pub transcript_renders: u32,
    pub composer_renders: u32,
    pub changes_renders: u32,
}

/// Rolling collector. Split from the clock so the arithmetic is unit-tested.
#[derive(Debug)]
pub struct FrameBudget {
    intervals: Vec<u64>,
    builds: Vec<u64>,
    children: Vec<u64>,
    totals: Vec<u64>,
    counts: [u32; 3],
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
            children: Vec::with_capacity(window),
            totals: Vec::with_capacity(window),
            counts: [0; 3],
            missed: 0,
            window: window.max(1),
        }
    }

    /// Record one frame. `interval_us` is `None` for the first frame of a run,
    /// after an idle gap, or while the window is inactive (gpui throttles it).
    /// `active` is the state of the frame being recorded. Returns a [`Summary`]
    /// once the window holds `window` samples — or `MIN_FLUSH` samples
    /// immediately after an idle gap, since a burst has just ended and waiting
    /// for the full window could mean never emitting at all. The samples are
    /// cleared either way.
    pub fn record(
        &mut self,
        interval_us: Option<u64>,
        build_us: u64,
        children_us: u64,
        counts: [u32; 3],
        active: bool,
    ) -> Option<Summary> {
        let interval_us = active.then_some(interval_us).flatten();
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
        self.children.push(children_us);
        self.totals.push(build_us.saturating_add(children_us));
        for (slot, count) in self.counts.iter_mut().zip(counts) {
            *slot = slot.saturating_add(count);
        }
        // The window is one frame long per build sample; idle and inactive
        // frames contribute a build but no interval, so the frame count can
        // trail it.
        if self.builds.len() < self.window && !(burst_ended && self.builds.len() >= MIN_FLUSH) {
            return None;
        }
        let mut intervals = std::mem::take(&mut self.intervals);
        let mut builds = std::mem::take(&mut self.builds);
        let mut children = std::mem::take(&mut self.children);
        let mut totals = std::mem::take(&mut self.totals);
        intervals.sort_unstable();
        let (build_p50_us, build_p95_us, build_p99_us, build_max_us) = stats(&mut builds);
        let (children_p50_us, children_p95_us, children_p99_us, children_max_us) =
            stats(&mut children);
        let (total_p50_us, total_p95_us, total_p99_us, total_max_us) = stats(&mut totals);
        let summary = Summary {
            frames: intervals.len(),
            missed: self.missed,
            interval_p50_us: percentile(&intervals, 50),
            interval_p95_us: percentile(&intervals, 95),
            interval_p99_us: percentile(&intervals, 99),
            interval_max_us: *intervals.last().unwrap_or(&0),
            build_p50_us,
            build_p95_us,
            build_p99_us,
            build_max_us,
            children_p50_us,
            children_p95_us,
            children_p99_us,
            children_max_us,
            total_p50_us,
            total_p95_us,
            total_p99_us,
            total_max_us,
            transcript_renders: self.counts[ChildView::Transcript as usize],
            composer_renders: self.counts[ChildView::Composer as usize],
            changes_renders: self.counts[ChildView::Changes as usize],
        };
        self.intervals = Vec::with_capacity(self.window);
        self.builds = Vec::with_capacity(self.window);
        self.children = Vec::with_capacity(self.window);
        self.totals = Vec::with_capacity(self.window);
        self.counts = [0; 3];
        self.missed = 0;
        Some(summary)
    }
}

/// Sorted percentiles plus the maximum for one sample vector.
fn stats(samples: &mut [u64]) -> (u64, u64, u64, u64) {
    samples.sort_unstable();
    (
        percentile(samples, 50),
        percentile(samples, 95),
        percentile(samples, 99),
        samples.last().copied().unwrap_or(0),
    )
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
    static LAST_ACTIVE: Cell<bool> = const { Cell::new(true) };
    /// Root body of the frame that just finished rendering.
    static PENDING_BUILD_US: Cell<u64> = const { Cell::new(0) };
    /// Child render bodies accumulated since the last frame start.
    static PENDING_CHILDREN_US: Cell<u64> = const { Cell::new(0) };
    static PENDING_RENDERS: Cell<[u32; 3]> = const { Cell::new([0; 3]) };
}

fn record(interval_us: Option<u64>, build: u64, children: u64, counts: [u32; 3], active: bool) {
    let summary = BUDGET.with(|b| {
        b.borrow_mut()
            .record(interval_us, build, children, counts, active)
    });
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
            children_p50_us = s.children_p50_us,
            children_p95_us = s.children_p95_us,
            children_p99_us = s.children_p99_us,
            children_max_us = s.children_max_us,
            total_p50_us = s.total_p50_us,
            total_p95_us = s.total_p95_us,
            total_p99_us = s.total_p99_us,
            total_max_us = s.total_max_us,
            transcript_renders = s.transcript_renders,
            composer_renders = s.composer_renders,
            changes_renders = s.changes_renders,
            "window frame budget"
        );
    }
}

/// Time the root render body. Returns `None` when the knob is off.
///
/// The sample is finalized at the start of the next frame, because child views
/// render after this guard drops. `window_active` excludes throttled
/// background-window intervals from the scored samples.
///
/// Bind it to a name that lives to the end of `render` (`let _frame = ...`):
/// the drop is what records the build time.
pub struct FrameGuard {
    start: Instant,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        PENDING_BUILD_US.set(self.start.elapsed().as_micros() as u64);
    }
}

/// Start a measurement for this frame, if enabled. Finalizes the previous one.
pub fn frame(window_active: bool) -> Option<FrameGuard> {
    if !enabled() {
        return None;
    }
    let now = Instant::now();
    let prev_start = LAST_START.with(|last| last.replace(Some(now)));
    let prev_active = LAST_ACTIVE.replace(window_active);
    let build = PENDING_BUILD_US.replace(0);
    let children = PENDING_CHILDREN_US.replace(0);
    let counts = PENDING_RENDERS.replace([0; 3]);
    if let Some(prev) = prev_start {
        let interval_us = now.saturating_duration_since(prev).as_micros() as u64;
        record(Some(interval_us), build, children, counts, prev_active);
    }
    Some(FrameGuard { start: now })
}

/// Time one child view's render body. Accumulated into the current frame and
/// counted per view; `None` when the knob is off.
pub struct ChildGuard {
    start: Instant,
    view: ChildView,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_micros() as u64;
        let children = PENDING_CHILDREN_US.get();
        PENDING_CHILDREN_US.set(children.saturating_add(elapsed));
        let mut counts = PENDING_RENDERS.get();
        let slot = self.view as usize;
        counts[slot] = counts[slot].saturating_add(1);
        PENDING_RENDERS.set(counts);
    }
}

/// Time a child view render, if enabled.
pub fn child_frame(view: ChildView) -> Option<ChildGuard> {
    if !enabled() {
        return None;
    }
    Some(ChildGuard {
        start: Instant::now(),
        view,
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
        assert!(
            budget
                .record(Some(200_000), 1_000, 0, [0; 3], true)
                .is_none()
        );
        // Two frames inside the burst, one over budget.
        budget.record(Some(8_000), 1_000, 0, [0; 3], true);
        let summary = budget
            .record(Some(30_000), 1_000, 0, [0; 3], true)
            .expect("window full");
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.missed, 1);
        assert_eq!(summary.interval_max_us, 30_000);
    }

    #[test]
    fn inactive_windows_are_not_scored() {
        let mut budget = FrameBudget::new(2);
        budget.record(Some(33_000), 1_000, 0, [0; 3], false);
        let summary = budget
            .record(Some(33_000), 1_000, 0, [0; 3], false)
            .expect("window full");
        assert_eq!(summary.frames, 0);
        assert_eq!(summary.missed, 0);
        // Build samples are still reported: the render cost is real.
        assert_eq!(summary.build_p50_us, 1_000);
    }

    #[test]
    fn first_frame_is_not_scored_and_clears_after_a_window() {
        let mut budget = FrameBudget::new(2);
        assert!(budget.record(None, 900, 0, [0; 3], true).is_none());
        let summary = budget
            .record(Some(16_000), 700, 0, [0; 3], true)
            .expect("window full");
        assert_eq!(summary.frames, 1);
        assert_eq!(summary.missed, 0);
        // Nearest rank: two build samples, so p50 lands on the upper one.
        assert_eq!(summary.build_p50_us, 900);
        assert_eq!(summary.build_max_us, 900);
        // The window resets: the next full one starts from zero.
        assert!(budget.record(Some(16_000), 500, 0, [0; 3], true).is_none());
        let next = budget
            .record(Some(16_000), 500, 0, [0; 3], true)
            .expect("second window");
        assert_eq!(next.frames, 2);
        assert_eq!(next.missed, 0);
        assert_eq!(next.transcript_renders, 0);
    }

    #[test]
    fn an_idle_gap_closes_a_partial_window() {
        let mut budget = FrameBudget::new(240);
        // A burst too short to fill the window: nothing is emitted yet.
        for _ in 0..40 {
            assert!(budget.record(Some(8_000), 1_000, 0, [0; 3], true).is_none());
        }
        // The pause ends the burst, so the partial window is emitted instead
        // of being lost. The pause itself is not one of the scored frames.
        let summary = budget
            .record(Some(500_000), 1_000, 0, [0; 3], true)
            .expect("burst closed");
        assert_eq!(summary.frames, 40);
        assert_eq!(summary.missed, 0);
        // And the samples are cleared for the next burst.
        assert!(budget.record(Some(8_000), 1_000, 0, [0; 3], true).is_none());
    }

    #[test]
    fn build_children_and_total_follow_the_samples() {
        let mut budget = FrameBudget::new(4);
        let mut summary = None;
        for (build, children) in [(100, 10), (400, 40), (200, 20), (300, 30)] {
            summary = budget.record(Some(16_000), build, children, [0; 3], true);
        }
        let summary = summary.expect("window full");
        assert_eq!(summary.frames, 4);
        assert_eq!(summary.build_p50_us, 300);
        assert_eq!(summary.build_p99_us, 400);
        assert_eq!(summary.build_max_us, 400);
        assert_eq!(summary.children_p50_us, 30);
        assert_eq!(summary.children_max_us, 40);
        assert_eq!(summary.total_p50_us, 330);
        assert_eq!(summary.total_max_us, 440);
    }

    #[test]
    fn child_render_counts_accumulate_and_reset_per_window() {
        let mut budget = FrameBudget::new(2);
        assert!(
            budget
                .record(Some(16_000), 100, 10, [1, 0, 2], true)
                .is_none()
        );
        let summary = budget
            .record(Some(16_000), 100, 10, [1, 3, 0], true)
            .expect("window full");
        assert_eq!(summary.transcript_renders, 2);
        assert_eq!(summary.composer_renders, 3);
        assert_eq!(summary.changes_renders, 2);
        assert!(budget.record(Some(16_000), 100, 10, [0; 3], true).is_none());
        let next = budget
            .record(Some(16_000), 100, 10, [0; 3], true)
            .expect("second window");
        assert_eq!(next.transcript_renders, 0);
    }
}
