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
//! `transcript_renders`, `composer_renders`, `changes_renders`,
//! `context_usage_renders` and `rail_renders` count child renders per window.
//! With gpui view caching enabled, a burst that only animates a Shell-owned
//! element must show zero renders for the models.
//!
//! Row renders are counted separately, keyed by `(kind tag, row id)`: the
//! window-level child count says *whether* the transcript re-rendered, the
//! per-row map says *which* lines it rebuilt. The map is dropped with the
//! window, and a row the transcript no longer lists is pruned at the next
//! frame boundary — a deleted line must not keep a stale count.
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
//! missed even though nothing is late. Their frames are skipped whole — no
//! interval, build time, child time or render count — because none of it is
//! comparable to a focused capture. Capture with the window focused.
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
use std::collections::{HashMap, HashSet};
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
    ContextUsage = 3,
    Rail = 4,
}

impl ChildView {
    /// Number of instrumented child views; sizes the per-frame counters.
    pub const COUNT: usize = 5;
}

/// Window key for one transcript row: a small kind tag (`RowKind` carries
/// `Arc`s and `SharedString`s, so the value itself is not hashable) plus the
/// row's stable id.
type RowKey = (&'static str, String);

/// `KOMET_WINDOW_STATS=1` enables the measurement. Zero cost when off.
fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("KOMET_WINDOW_STATS").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// One logged window of samples.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub context_usage_renders: u32,
    pub rail_renders: u32,
    /// Rows that re-rendered in this window, most-rendered first.
    pub rows: Vec<RowRenders>,
}

impl Summary {
    /// Total row renders in this window, across every row.
    pub fn row_renders(&self) -> u32 {
        self.rows.iter().map(|row| row.renders).sum()
    }
}

/// One transcript row's render count in a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRenders {
    /// Row kind tag (`user`, `markdown`, `tools`, `compaction`, …).
    pub kind: &'static str,
    pub id: String,
    pub renders: u32,
}

/// Row renders between two frame boundaries, plus the transcript's live row
/// ids at capture time. `live` is `None` until the transcript reports its
/// rows (and in the collector's own tests, which have no transcript); a row
/// missing from a reported set is pruned from the window map, so a deleted
/// line does not keep a stale count.
#[derive(Debug, Default)]
pub struct FrameRows {
    pub counts: HashMap<RowKey, u32>,
    pub live: Option<HashSet<String>>,
}

/// Rolling collector. Split from the clock so the arithmetic is unit-tested.
#[derive(Debug)]
pub struct FrameBudget {
    intervals: Vec<u64>,
    builds: Vec<u64>,
    children: Vec<u64>,
    totals: Vec<u64>,
    counts: [u32; ChildView::COUNT],
    rows: HashMap<RowKey, u32>,
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
            counts: [0; ChildView::COUNT],
            rows: HashMap::new(),
            missed: 0,
            window: window.max(1),
        }
    }

    /// Record one frame. `interval_us` is `None` for the first frame of a run
    /// or after an idle gap. Inactive frames are skipped entirely: gpui
    /// throttles the window, so their interval, render cost and child renders
    /// are not comparable to a focused capture. Returns a [`Summary`] once the
    /// window holds `window` samples — or `MIN_FLUSH` samples immediately
    /// after an idle gap, since a burst has just ended and waiting for the
    /// full window could mean never emitting at all. The samples are cleared
    /// either way.
    pub fn record(
        &mut self,
        interval_us: Option<u64>,
        build_us: u64,
        children_us: u64,
        counts: [u32; ChildView::COUNT],
        rows: FrameRows,
        active: bool,
    ) -> Option<Summary> {
        if !active {
            return None;
        }
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
        let FrameRows {
            counts: frame_rows,
            live,
        } = rows;
        if let Some(live) = live {
            self.rows.retain(|(_, id), _| live.contains(id));
        }
        for (key, count) in frame_rows {
            let slot = self.rows.entry(key).or_default();
            *slot = slot.saturating_add(count);
        }
        // The window is one frame long per build sample; an idle-gap frame
        // contributes a build but no interval, so the frame count can trail it.
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
        let mut rows: Vec<RowRenders> = std::mem::take(&mut self.rows)
            .into_iter()
            .filter(|(_, renders)| *renders > 0)
            .map(|((kind, id), renders)| RowRenders { kind, id, renders })
            .collect();
        rows.sort_by(|a, b| {
            b.renders
                .cmp(&a.renders)
                .then_with(|| a.kind.cmp(b.kind))
                .then_with(|| a.id.cmp(&b.id))
        });
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
            context_usage_renders: self.counts[ChildView::ContextUsage as usize],
            rail_renders: self.counts[ChildView::Rail as usize],
            rows,
        };
        self.intervals = Vec::with_capacity(self.window);
        self.builds = Vec::with_capacity(self.window);
        self.children = Vec::with_capacity(self.window);
        self.totals = Vec::with_capacity(self.window);
        self.counts = [0; ChildView::COUNT];
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
    static PENDING_RENDERS: Cell<[u32; ChildView::COUNT]> =
        const { Cell::new([0; ChildView::COUNT]) };
    /// Rows rendered since the last frame start, keyed by `(kind tag, id)`.
    static PENDING_ROWS: RefCell<HashMap<RowKey, u32>> = RefCell::new(HashMap::new());
    /// The transcript's live row ids as of its last render; `None` until it
    /// reports (a cached transcript keeps the previous set, which is still
    /// the live set).
    static LIVE_ROWS: RefCell<Option<HashSet<String>>> = const { RefCell::new(None) };
}

fn record(
    interval_us: Option<u64>,
    build: u64,
    children: u64,
    counts: [u32; ChildView::COUNT],
    rows: FrameRows,
    active: bool,
) {
    let summary = BUDGET.with(|b| {
        b.borrow_mut()
            .record(interval_us, build, children, counts, rows, active)
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
            context_usage_renders = s.context_usage_renders,
            rail_renders = s.rail_renders,
            row_renders = s.row_renders(),
            "window frame budget"
        );
        for row in &s.rows {
            tracing::warn!(
                kind = row.kind,
                id = %row.id,
                renders = row.renders,
                "row frame budget"
            );
        }
    }
}

/// Time the root render body. Returns `None` when the knob is off.
///
/// The sample is finalized at the start of the next frame, because child views
/// render after this guard drops. Frames rendered while the window was
/// inactive are dropped whole by [`FrameBudget::record`] — gpui throttles
/// them, so neither their interval nor their render cost is comparable.
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
    let counts = PENDING_RENDERS.replace([0; ChildView::COUNT]);
    let rows = PENDING_ROWS.with(|rows| std::mem::take(&mut *rows.borrow_mut()));
    let live = LIVE_ROWS.with(|live| live.borrow().clone());
    if let Some(prev) = prev_start {
        let interval_us = now.saturating_duration_since(prev).as_micros() as u64;
        record(
            Some(interval_us),
            build,
            children,
            counts,
            FrameRows { counts: rows, live },
            prev_active,
        );
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

/// Count one transcript row's render, keyed by a small kind tag and the row's
/// stable id. `RowKind` carries `Arc`s and `SharedString`s — pass
/// [`RowTag`]-style constants (e.g. `"compaction"`), not the value. `None`
/// when the knob is off, so the call site pays one env-free branch.
pub struct RowGuard {
    kind: &'static str,
    id: String,
}

impl Drop for RowGuard {
    fn drop(&mut self) {
        PENDING_ROWS.with(|rows| {
            let mut rows = rows.borrow_mut();
            let slot = rows.entry((self.kind, self.id.clone())).or_default();
            *slot = slot.saturating_add(1);
        });
    }
}

/// Count a transcript row render, if enabled.
pub fn row_frame(kind: &'static str, id: &str) -> Option<RowGuard> {
    if !enabled() {
        return None;
    }
    Some(RowGuard {
        kind,
        id: id.to_owned(),
    })
}

/// Whether per-row counters are active — lets the transcript skip collecting
/// its live ids when the knob is off.
pub fn rows_enabled() -> bool {
    enabled()
}

/// Record the transcript's live row ids for this frame. Ids missing from the
/// next reported set are pruned from the window map; no-op when the knob is
/// off.
pub fn rows_live(ids: &[&str]) {
    if !enabled() {
        return;
    }
    LIVE_ROWS.with(|live| {
        *live.borrow_mut() = Some(ids.iter().map(|id| (*id).to_owned()).collect());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three-view counters padded with the newer views zeroed.
    fn counts(three: [u32; 3]) -> [u32; ChildView::COUNT] {
        let mut all = [0; ChildView::COUNT];
        all[..3].copy_from_slice(&three);
        all
    }

    /// One render per `(kind, id)` entry, no live-row report.
    fn frame_rows(entries: &[(&'static str, &str)]) -> FrameRows {
        let mut counts = HashMap::new();
        for (kind, id) in entries {
            *counts.entry((*kind, (*id).to_owned())).or_default() += 1;
        }
        FrameRows { counts, live: None }
    }

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
                .record(
                    Some(200_000),
                    1_000,
                    0,
                    counts([0; 3]),
                    FrameRows::default(),
                    true
                )
                .is_none()
        );
        // Two frames inside the burst, one over budget.
        budget.record(
            Some(8_000),
            1_000,
            0,
            counts([0; 3]),
            FrameRows::default(),
            true,
        );
        let summary = budget
            .record(
                Some(30_000),
                1_000,
                0,
                counts([0; 3]),
                FrameRows::default(),
                true,
            )
            .expect("window full");
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.missed, 1);
        assert_eq!(summary.interval_max_us, 30_000);
    }

    #[test]
    fn inactive_windows_contribute_nothing() {
        let mut budget = FrameBudget::new(2);
        // Throttled frames are dropped whole: no interval, build, child time
        // or render count — the window does not even advance.
        assert!(
            budget
                .record(
                    Some(33_000),
                    1_000,
                    100,
                    counts([3, 2, 1]),
                    frame_rows(&[("user", "a")]),
                    false
                )
                .is_none()
        );
        assert!(budget.intervals.is_empty());
        assert!(budget.builds.is_empty());
        assert!(budget.children.is_empty());
        assert_eq!(budget.counts, [0; ChildView::COUNT]);
        assert!(budget.rows.is_empty());
        // The next focused frames alone fill the window.
        assert!(
            budget
                .record(
                    Some(16_000),
                    500,
                    50,
                    counts([1, 0, 0]),
                    FrameRows::default(),
                    true
                )
                .is_none()
        );
        let summary = budget
            .record(
                Some(16_000),
                600,
                60,
                counts([1, 0, 0]),
                FrameRows::default(),
                true,
            )
            .expect("window full");
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.missed, 0);
        assert_eq!(summary.build_p50_us, 600);
        assert_eq!(summary.children_p50_us, 60);
        assert_eq!(summary.transcript_renders, 2);
    }

    #[test]
    fn first_frame_is_not_scored_and_clears_after_a_window() {
        let mut budget = FrameBudget::new(2);
        assert!(
            budget
                .record(None, 900, 0, counts([0; 3]), FrameRows::default(), true)
                .is_none()
        );
        let summary = budget
            .record(
                Some(16_000),
                700,
                0,
                counts([0; 3]),
                FrameRows::default(),
                true,
            )
            .expect("window full");
        assert_eq!(summary.frames, 1);
        assert_eq!(summary.missed, 0);
        // Nearest rank: two build samples, so p50 lands on the upper one.
        assert_eq!(summary.build_p50_us, 900);
        assert_eq!(summary.build_max_us, 900);
        // The window resets: the next full one starts from zero.
        assert!(
            budget
                .record(
                    Some(16_000),
                    500,
                    0,
                    counts([0; 3]),
                    FrameRows::default(),
                    true
                )
                .is_none()
        );
        let next = budget
            .record(
                Some(16_000),
                500,
                0,
                counts([0; 3]),
                FrameRows::default(),
                true,
            )
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
            assert!(
                budget
                    .record(
                        Some(8_000),
                        1_000,
                        0,
                        counts([0; 3]),
                        FrameRows::default(),
                        true
                    )
                    .is_none()
            );
        }
        // The pause ends the burst, so the partial window is emitted instead
        // of being lost. The pause itself is not one of the scored frames.
        let summary = budget
            .record(
                Some(500_000),
                1_000,
                0,
                counts([0; 3]),
                FrameRows::default(),
                true,
            )
            .expect("burst closed");
        assert_eq!(summary.frames, 40);
        assert_eq!(summary.missed, 0);
        // And the samples are cleared for the next burst.
        assert!(
            budget
                .record(
                    Some(8_000),
                    1_000,
                    0,
                    counts([0; 3]),
                    FrameRows::default(),
                    true
                )
                .is_none()
        );
    }

    #[test]
    fn build_children_and_total_follow_the_samples() {
        let mut budget = FrameBudget::new(4);
        let mut summary = None;
        for (build, children) in [(100, 10), (400, 40), (200, 20), (300, 30)] {
            summary = budget.record(
                Some(16_000),
                build,
                children,
                counts([0; 3]),
                FrameRows::default(),
                true,
            );
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
                .record(
                    Some(16_000),
                    100,
                    10,
                    counts([1, 0, 2]),
                    FrameRows::default(),
                    true
                )
                .is_none()
        );
        let summary = budget
            .record(
                Some(16_000),
                100,
                10,
                counts([1, 3, 0]),
                FrameRows::default(),
                true,
            )
            .expect("window full");
        assert_eq!(summary.transcript_renders, 2);
        assert_eq!(summary.composer_renders, 3);
        assert_eq!(summary.changes_renders, 2);
        assert!(
            budget
                .record(
                    Some(16_000),
                    100,
                    10,
                    counts([0; 3]),
                    FrameRows::default(),
                    true
                )
                .is_none()
        );
        let next = budget
            .record(
                Some(16_000),
                100,
                10,
                counts([0; 3]),
                FrameRows::default(),
                true,
            )
            .expect("second window");
        assert_eq!(next.transcript_renders, 0);
    }

    #[test]
    fn per_view_and_per_row_counters_count_once_and_leave_neighbours_alone() {
        let mut budget = FrameBudget::new(2);
        let mut views = counts([0; 3]);
        views[ChildView::ContextUsage as usize] = 1;
        assert!(
            budget
                .record(
                    Some(16_000),
                    100,
                    10,
                    views,
                    frame_rows(&[("tools", "call_1"), ("tools", "call_2")]),
                    true
                )
                .is_none()
        );
        let summary = budget
            .record(
                Some(16_000),
                100,
                10,
                counts([0; 3]),
                FrameRows::default(),
                true,
            )
            .expect("window full");
        assert_eq!(summary.context_usage_renders, 1);
        assert_eq!(summary.rail_renders, 0);
        assert_eq!(summary.transcript_renders, 0);
        assert_eq!(summary.composer_renders, 0);
        assert_eq!(summary.changes_renders, 0);
        // Two distinct rows, one render each — and the window map is drained.
        assert_eq!(summary.row_renders(), 2);
        assert_eq!(summary.rows.len(), 2);
        assert!(
            summary
                .rows
                .iter()
                .all(|row| row.kind == "tools" && row.renders == 1)
        );
        assert!(budget.rows.is_empty());
    }

    #[test]
    fn removed_rows_are_pruned_at_the_next_frame_boundary() {
        let mut budget = FrameBudget::new(3);
        budget.record(
            Some(16_000),
            100,
            10,
            counts([0; 3]),
            frame_rows(&[("user", "gone"), ("user", "kept")]),
            true,
        );
        // The transcript now lists only `kept`: `gone` must not keep a count.
        let live = FrameRows {
            counts: HashMap::new(),
            live: Some(["kept".to_owned()].into_iter().collect()),
        };
        budget.record(Some(16_000), 100, 10, counts([0; 3]), live, true);
        let summary = budget
            .record(
                Some(16_000),
                100,
                10,
                counts([0; 3]),
                frame_rows(&[("user", "kept")]),
                true,
            )
            .expect("window full");
        assert_eq!(summary.rows.len(), 1);
        assert_eq!(summary.rows[0].id, "kept");
        assert_eq!(summary.rows[0].renders, 2);
    }

    #[test]
    fn row_and_child_counters_are_inert_when_the_knob_is_off() {
        // The env switch latches once per process, so only assert in a
        // capture-free test run (CI never sets KOMET_WINDOW_STATS).
        if enabled() {
            return;
        }
        assert!(child_frame(ChildView::ContextUsage).is_none());
        assert!(child_frame(ChildView::Rail).is_none());
        assert!(row_frame("user", "row").is_none());
        assert!(!rows_enabled());
        rows_live(&["row"]);
        assert!(LIVE_ROWS.with(|live| live.borrow().is_none()));
        assert!(PENDING_ROWS.with(|rows| rows.borrow().is_empty()));
    }
}
