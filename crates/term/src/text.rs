//! Plain-text reads on top of the emulator, stamped with the engine's output
//! sequence numbers.
//!
//! A caller without a screen — the engine, and past it an agent asking "what is
//! this terminal showing" — needs three things, and this module gives all three
//! in one type ([`TerminalMirror`]):
//!
//! 1. the current screen as text ([`Emulator::screen_text`], re-exposed), which
//!    is the *only* sane read for a full-screen app: its scrollback is a pile of
//!    escape drafts, its display is the viewport;
//! 2. the last N lines of the transcript ([`Emulator::tail_text`]);
//! 3. what appeared since a given `seq` ([`TextJournal::delta_since`]).
//!
//! The journal derives (3) by comparing the tail window before and after each
//! output batch. Two grid changes are *not* new text and are handled as such:
//! the viewport scrolling (lines move up, nothing was printed) and a program
//! rewriting the screen in place (a redraw, a `clear`, the shell editing its
//! prompt). The first is folded away by matching a shifted window; the second is
//! reported with [`TextDelta::needs_snapshot`], because the honest answer to
//! "what changed since seq" after a rewrite is "re-read the screen" — a silently
//! wrong rolling transcript is the failure mode this avoids.

use std::collections::VecDeque;

use crate::emulator::{Emulator, GridSize};
use crate::markers::CommandBlock;

/// Lines of tail text compared at each output step. Scrolling and rewrites are
/// only recognizable inside this window, and it bounds what one journal entry
/// can cost. 200 lines covers a screenful of any sane terminal several times
/// over.
pub const TRACKED_TAIL_WINDOW: usize = 200;

/// How many output steps the journal remembers before the oldest deltas become
/// unavailable ([`TextDelta::truncated`]). At the engine's 12 ms batching this
/// is a short window, which is the right trade: a caller that needs more re-reads
/// the screen or the tail rather than getting a half-truth.
pub const TRACKED_JOURNAL_STEPS: usize = 64;

/// One output step: what [`TextJournal::record`] made of the bytes stamped
/// `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextStep {
    /// The engine's output sequence number for the batch that produced this.
    pub seq: u64,
    /// Text that appeared. With `reset` set this is the region (or, after
    /// [`TextJournal::invalidate`], the whole window) as it now reads, not a
    /// list of new lines.
    pub lines: Vec<String>,
    /// The step could not be expressed as "lines appended to the tail": the
    /// program rewrote the screen, the window scrolled past the tail, the grid
    /// was resized, or the alt screen flipped.
    pub reset: bool,
}

/// The answer to "what appeared since `seq`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDelta {
    /// The sequence number the caller asked about.
    pub from_seq: u64,
    /// The newest step folded into this delta (`0` when the journal is empty).
    pub through_seq: u64,
    /// Text that appeared, oldest first. Empty when `from_seq` is already the
    /// newest state.
    pub lines: Vec<String>,
    /// Something rewrote the screen rather than appending to it (redraw, `clear`,
    /// prompt editing, resize, alt-screen flip): `lines` is what the rewritten
    /// region now reads, and the caller should take [`Emulator::screen_text`]
    /// instead of applying it as an append. Set even when `lines` is empty — the
    /// rewrite may be stamped at the caller's own seq.
    pub needs_snapshot: bool,
    /// Steps older than [`TextJournal::oldest_seq`] were dropped, so this delta
    /// is missing part of the range it was asked for.
    pub truncated: bool,
}

impl TextDelta {
    fn empty(from_seq: u64, through_seq: u64) -> Self {
        Self {
            from_seq,
            through_seq,
            lines: Vec::new(),
            needs_snapshot: false,
            truncated: false,
        }
    }
}

/// The seq-stamped tail-window journal. Fed by [`TerminalMirror::feed`]; usable
/// on its own for tests that do not want an emulator.
#[derive(Debug, Clone)]
pub struct TextJournal {
    window: usize,
    max_steps: usize,
    /// The normalized tail window as of the last recorded step.
    last_text: Vec<String>,
    steps: VecDeque<TextStep>,
    latest_seq: Option<u64>,
    /// Set by [`Self::invalidate`]: the next record re-reports instead of
    /// diffing.
    pending_reset: bool,
}

impl Default for TextJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl TextJournal {
    pub fn new() -> Self {
        Self::with_limits(TRACKED_TAIL_WINDOW, TRACKED_JOURNAL_STEPS)
    }

    pub fn with_limits(window: usize, max_steps: usize) -> Self {
        Self {
            window: window.max(1),
            max_steps: max_steps.max(1),
            last_text: Vec::new(),
            steps: VecDeque::new(),
            latest_seq: None,
            pending_reset: false,
        }
    }

    /// The window this journal compares, for callers that want to hand it
    /// exactly `tail_text(journal.window())` lines.
    pub fn window(&self) -> usize {
        self.window
    }

    /// Fold the terminal's current tail text into the journal as step `seq`.
    /// `text` is newest-last (what [`Emulator::tail_text`] returns); trailing
    /// blank lines are dropped first, because the unused rows at the bottom of a
    /// viewport are padding, not content, and leaving them in would make every
    /// write onto a blank row look like a rewrite.
    pub fn record(&mut self, seq: u64, text: &[String]) {
        let tail = normalize(Self::window_of(text, self.window));
        let (lines, reset) = if self.pending_reset {
            (tail.clone(), true)
        } else {
            match appended_lines(&self.last_text, &tail) {
                Some(lines) => (lines, false),
                // No shift explains the change: report the region that now reads
                // differently and tell the caller to take a snapshot.
                None => (rewritten_region(&self.last_text, &tail), true),
            }
        };
        self.pending_reset = false;
        self.steps.push_back(TextStep { seq, lines, reset });
        while self.steps.len() > self.max_steps {
            self.steps.pop_front();
        }
        self.last_text = tail;
        self.latest_seq = Some(seq);
    }

    /// Mark the screen as no longer comparable to its previous text (a resize
    /// re-wraps every line; entering or leaving the alt screen swaps buffers) and
    /// record the current window as a re-report.
    pub fn invalidate(&mut self, seq: u64, text: &[String]) {
        self.pending_reset = true;
        self.record(seq, text);
    }

    /// Everything that appeared after `from_seq`. A step *is* the state at its
    /// own `seq`, so asking "since seq" returns the steps with `seq >` the
    /// argument and an empty delta when the caller is already caught up.
    pub fn delta_since(&self, from_seq: u64) -> TextDelta {
        let through = self.latest_seq.unwrap_or(from_seq);
        if self.steps.is_empty() {
            return TextDelta::empty(from_seq, self.latest_seq.unwrap_or(0));
        }
        if from_seq >= through {
            let mut caught_up = TextDelta::empty(from_seq, through);
            // The newest state may itself be a re-report — a resize or an
            // alt-screen flip is stamped at the newest *output* seq (it is not
            // output, so it mints no seq of its own). A caller holding the text
            // from that output still has to be told its screen is stale.
            caught_up.needs_snapshot = self.steps.back().is_some_and(|step| step.reset);
            return caught_up;
        }
        let oldest = self.steps.front().expect("non-empty").seq;
        let mut delta = TextDelta {
            from_seq,
            through_seq: through,
            lines: Vec::new(),
            needs_snapshot: false,
            // Steps with seq in (from_seq, oldest) existed and are gone.
            truncated: from_seq + 1 < oldest,
        };
        for step in self.steps.iter().filter(|step| step.seq > from_seq) {
            if step.reset {
                // Text accumulated before a rewrite no longer describes the
                // screen: drop it and say so.
                delta.needs_snapshot = true;
                delta.lines.clear();
            }
            delta.lines.extend(step.lines.iter().cloned());
        }
        delta
    }

    /// The newest recorded step, if any.
    pub fn latest_seq(&self) -> Option<u64> {
        self.latest_seq
    }

    /// The oldest `seq` this journal can still describe fully.
    pub fn oldest_seq(&self) -> Option<u64> {
        self.steps.front().map(|step| step.seq)
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// The last `window` lines, newest-last, as a slice.
    fn window_of(text: &[String], window: usize) -> &[String] {
        &text[text.len().saturating_sub(window)..]
    }
}

/// Drop the trailing blank lines of a tail window (see [`TextJournal::record`]).
fn normalize(text: &[String]) -> Vec<String> {
    let end = text.iter().rposition(|line| !line.is_empty());
    match end {
        Some(end) => text[..=end].to_vec(),
        None => Vec::new(),
    }
}

/// The lines that were appended to the tail window, or `None` when no shift
/// explains the change.
///
/// A terminal appends at the bottom and, once the screen is full, drops from the
/// top of history. So the previous window is a *shifted* prefix of the new one:
/// `old[k..] == new[..old.len() - k]` for the scroll count `k`. Trying `k` in
/// increasing order and rejecting a wrong shift on its first line keeps the
/// common cases linear (pure append `k = 0`, one-line scroll `k = 1`). A shift
/// of `old.len()` (the whole window replaced) is deliberately not treated as an
/// append: there is nothing left to anchor on, so it is a rewrite.
fn appended_lines(old: &[String], new: &[String]) -> Option<Vec<String>> {
    if old.is_empty() {
        return Some(new.to_vec());
    }
    for shift in 0..old.len() {
        if shift > 0 && old[shift] != new[0] {
            continue;
        }
        let keep = old.len() - shift;
        if new.len() < keep || old[shift..] != new[..keep] {
            continue;
        }
        return Some(new[keep..].to_vec());
    }
    None
}

/// The new text inside the changed region: longest common prefix and suffix
/// trimmed off. Used when [`appended_lines`] gives up, so a rewrite still
/// reports *something* rather than nothing — with `needs_snapshot` telling the
/// caller not to treat it as an append.
fn rewritten_region(old: &[String], new: &[String]) -> Vec<String> {
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(before, after)| before == after)
        .count();
    let max_suffix = old
        .len()
        .saturating_sub(prefix)
        .min(new.len().saturating_sub(prefix));
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(before, after)| before == after)
        .count()
        .min(max_suffix);
    new[prefix..new.len() - suffix].to_vec()
}

/// An emulator plus its [`TextJournal`]: the engine's mirror of what a terminal
/// displays, fed from the PTY pump and read as plain text.
///
/// The pairing lives here because the journal only stays correct if it is
/// stamped at the same moments the grid changes: every output batch, every
/// resize, and every alt-screen flip (which the mirror watches for itself).
#[derive(Debug)]
pub struct TerminalMirror {
    emulator: Emulator,
    journal: TextJournal,
    /// Whether the alt screen was active at the last stamp, to notice a flip
    /// hidden inside an output batch.
    alt_screen: bool,
    /// The newest output sequence number folded into the mirror.
    seq: u64,
}

impl TerminalMirror {
    pub fn new(emulator: Emulator) -> Self {
        Self::with_journal(emulator, TextJournal::new())
    }

    pub fn with_journal(emulator: Emulator, journal: TextJournal) -> Self {
        let alt_screen = emulator.mouse_modes().alt_screen;
        Self {
            emulator,
            journal,
            alt_screen,
            seq: 0,
        }
    }

    /// The emulated grid, for callers that want cells, colors, or the modes.
    pub fn emulator(&self) -> &Emulator {
        &self.emulator
    }

    pub fn cols(&self) -> usize {
        self.emulator.cols()
    }

    pub fn rows(&self) -> usize {
        self.emulator.rows()
    }

    /// The newest output sequence number folded in.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Feed one output batch stamped `seq` and record the text it produced.
    /// Returns the bytes the terminal asks written back to the PTY, exactly as
    /// [`Emulator::feed`].
    pub fn feed(&mut self, seq: u64, bytes: &[u8]) -> Vec<u8> {
        let responses = self.emulator.feed(bytes);
        self.stamp(seq);
        responses
    }

    /// Resize the emulated grid and mark the text as re-wrapped: every line may
    /// now read differently, so a delta spanning this step cannot be an append.
    pub fn resize(&mut self, seq: u64, cols: u16, rows: u16) {
        let size = GridSize::new(cols, rows);
        self.emulator.resize(size.cols, size.rows);
        self.stamp_reset(seq);
    }

    /// Request 1: the current screen in clear text, top to bottom.
    pub fn screen_text(&self) -> Vec<String> {
        self.emulator.screen_text()
    }

    /// Request 2: the last `lines` lines in clear text, oldest first.
    pub fn tail_text(&self, lines: usize) -> Vec<String> {
        self.emulator.tail_text(lines)
    }

    /// Request 3: the text that appeared after `from_seq`.
    pub fn delta_since(&self, from_seq: u64) -> TextDelta {
        self.journal.delta_since(from_seq)
    }

    /// The OSC title, which the engine cannot see any other way.
    pub fn title(&self) -> Option<&str> {
        self.emulator.title()
    }

    /// Re-baseline the journal on the current screen without feeding bytes: the
    /// next delta starts from here.
    pub fn resync(&mut self, seq: u64) {
        self.stamp_reset(seq);
    }

    // ---- OSC 133 command blocks (pass-throughs onto the emulated grid) ----
    //
    // The engine's `last_command` read speaks these; the journal above stays the delta mechanism, blocks stay the
    // boundary mechanism. Thin wrappers on purpose: the policy lives in
    // [`Emulator`], the mirror only owns the grid the engine picked.

    /// The command blocks this mirror describes (see [`Emulator::command_blocks`]).
    pub fn command_blocks(&self) -> Vec<CommandBlock> {
        self.emulator.command_blocks()
    }

    /// The newest block: the running command, or the last one that finished.
    pub fn last_command_block(&self) -> Option<CommandBlock> {
        self.emulator.last_command_block()
    }

    /// The text a block's output region holds, oldest line first.
    pub fn block_lines(&self, block: &CommandBlock) -> Vec<String> {
        self.emulator.block_lines(block)
    }

    /// Did a valid OSC 133 marker ever arrive? Without one there is no honest
    /// command boundary (see [`Emulator::saw_markers`]).
    pub fn saw_markers(&self) -> bool {
        self.emulator.saw_markers()
    }

    fn stamp(&mut self, seq: u64) {
        let alt_screen = self.emulator.mouse_modes().alt_screen;
        let text = self.emulator.tail_text(self.journal.window());
        if alt_screen != self.alt_screen {
            self.journal.invalidate(seq, &text);
            self.alt_screen = alt_screen;
        } else {
            self.journal.record(seq, &text);
        }
        self.seq = seq;
    }

    fn stamp_reset(&mut self, seq: u64) {
        let text = self.emulator.tail_text(self.journal.window());
        self.journal.invalidate(seq, &text);
        self.alt_screen = self.emulator.mouse_modes().alt_screen;
        self.seq = seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(cols: u16, rows: u16) -> TerminalMirror {
        TerminalMirror::new(Emulator::new(cols, rows))
    }

    /// Feed the way the engine does: one seq per output batch.
    fn feed(mirror: &mut TerminalMirror, seq: u64, bytes: &[u8]) {
        mirror.feed(seq, bytes);
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn appended_lines_folds_a_pure_append() {
        let old = lines(&["a", "b"]);
        let new = lines(&["a", "b", "c"]);
        assert_eq!(appended_lines(&old, &new), Some(lines(&["c"])));
    }

    #[test]
    fn appended_lines_folds_a_scrolled_window() {
        // The viewport scrolled by one: nothing new above, one line at the end.
        let old = lines(&["l1", "l2", "l3"]);
        let new = lines(&["l2", "l3", "l4"]);
        assert_eq!(appended_lines(&old, &new), Some(lines(&["l4"])));
    }

    #[test]
    fn appended_lines_rejects_a_mid_window_rewrite() {
        let old = lines(&["a", "b", "c"]);
        let new = lines(&["a", "X", "c"]);
        assert_eq!(appended_lines(&old, &new), None);
    }

    #[test]
    fn appended_lines_never_indexes_past_the_window() {
        // A completely replaced window used to be an out-of-bounds shift.
        let old = lines(&["a"]);
        let new = lines(&["b"]);
        assert_eq!(appended_lines(&old, &new), None);
        let empty: Vec<String> = Vec::new();
        assert_eq!(appended_lines(&empty, &new), Some(lines(&["b"])));
    }

    #[test]
    fn rewritten_region_trims_the_unchanged_edges() {
        let old = lines(&["top", "one", "bottom"]);
        let new = lines(&["top", "two", "three", "bottom"]);
        assert_eq!(rewritten_region(&old, &new), lines(&["two", "three"]));
        // Total replacement keeps the whole new window.
        assert_eq!(rewritten_region(&old, &lines(&["x"])), lines(&["x"]));
    }

    #[test]
    fn record_ignores_trailing_blank_viewport_rows() {
        let mut j = TextJournal::new();
        j.record(1, &lines(&["prompt", "", ""]));
        assert_eq!(j.delta_since(0).lines, lines(&["prompt"]));
        // Writing on the blank row below is an append, not a rewrite.
        j.record(2, &lines(&["prompt", "next", ""]));
        let delta = j.delta_since(1);
        assert!(!delta.needs_snapshot);
        assert_eq!(delta.lines, lines(&["next"]));
    }

    #[test]
    fn journal_reports_nothing_when_the_caller_is_caught_up() {
        let mut j = TextJournal::new();
        j.record(1, &lines(&["prompt"]));
        assert_eq!(j.delta_since(1).lines, Vec::<String>::new());
        assert!(!j.delta_since(1).truncated);
        assert!(!j.delta_since(1).needs_snapshot);
        // An even newer seq reads as caught up too, not as a negative delta.
        assert_eq!(j.delta_since(9).through_seq, 1);
        // Nothing recorded at all: empty and honest, not truncated.
        let fresh = TextJournal::new();
        assert!(fresh.delta_since(0).lines.is_empty());
        assert!(!fresh.delta_since(0).truncated);
    }

    #[test]
    fn journal_deltas_only_cover_what_followed_the_seq() {
        let mut j = TextJournal::new();
        j.record(1, &lines(&["one"]));
        j.record(2, &lines(&["one", "two"]));
        j.record(3, &lines(&["one", "two", "three"]));
        let from_2 = j.delta_since(2);
        assert_eq!(from_2.lines, lines(&["three"]));
        assert_eq!(from_2.through_seq, 3);
        assert!(!from_2.needs_snapshot);
        assert!(!from_2.truncated);
        // From the very beginning: everything, in order.
        assert_eq!(j.delta_since(0).lines, lines(&["one", "two", "three"]));
    }

    #[test]
    fn a_re_report_at_the_callers_own_seq_still_flags_a_stale_screen() {
        // The engine stamps a resize at the newest *output* seq (a resize is not
        // output and mints no seq), so a caller that is "caught up" can still be
        // holding text that no longer reads the same.
        let mut j = TextJournal::new();
        j.record(1, &lines(&["a"]));
        j.invalidate(1, &lines(&["b"]));
        let delta = j.delta_since(1);
        assert!(delta.lines.is_empty(), "nothing followed seq 1");
        assert!(
            delta.needs_snapshot,
            "but the text held at seq 1 is no longer the screen"
        );
    }

    #[test]
    fn journal_prunes_its_oldest_steps_and_says_so() {
        let mut j = TextJournal::with_limits(8, 3);
        for seq in 1..=6u64 {
            let text: Vec<String> = (1..=seq).map(|i| format!("line{i}")).collect();
            j.record(seq, &text);
        }
        assert_eq!(j.len(), 3);
        assert_eq!(j.oldest_seq(), Some(4));
        assert_eq!(j.latest_seq(), Some(6));
        let delta = j.delta_since(1);
        assert!(delta.truncated, "steps 2 and 3 are gone");
        // Still reports what it does hold rather than nothing.
        assert_eq!(delta.lines, lines(&["line4", "line5", "line6"]));
        // A caller inside the retained range gets a clean delta.
        let fresh = j.delta_since(4);
        assert!(!fresh.truncated);
        assert_eq!(fresh.lines, lines(&["line5", "line6"]));
    }

    #[test]
    fn a_rewrite_marks_the_delta_as_needing_a_snapshot() {
        let mut j = TextJournal::new();
        j.record(1, &lines(&["one", "two"]));
        // In-place rewrite of the first line: no append explains it.
        j.record(2, &lines(&["ONE", "two"]));
        let delta = j.delta_since(1);
        assert!(delta.needs_snapshot);
        assert_eq!(delta.lines, lines(&["ONE"]));
        // Steps after a rewrite are the region, not a rolling transcript.
        j.record(3, &lines(&["ONE", "two", "three"]));
        let delta = j.delta_since(1);
        assert!(delta.needs_snapshot);
        assert_eq!(delta.lines, lines(&["ONE", "three"]));
    }

    #[test]
    fn invalidate_forces_a_full_re_report() {
        let mut j = TextJournal::new();
        j.record(1, &lines(&["a"]));
        j.invalidate(2, &lines(&["b", "c"]));
        let delta = j.delta_since(1);
        assert!(delta.needs_snapshot);
        assert_eq!(delta.lines, lines(&["b", "c"]));
    }

    #[test]
    fn the_window_bounds_what_one_step_can_hold() {
        let mut j = TextJournal::with_limits(2, 4);
        assert_eq!(j.window(), 2);
        j.record(1, &lines(&["a", "b", "c"]));
        assert_eq!(j.delta_since(0).lines, lines(&["b", "c"]));
    }

    // ---- the three reads, on a scripted byte stream ----

    #[test]
    fn a_shell_prompt_then_output_reads_as_a_transcript() {
        let mut m = mirror(40, 4);
        feed(&mut m, 1, b"$ ");
        // Grid padding is trimmed on read, so the prompt's trailing space is
        // gone: the text is what a copy of that line would give.
        assert_eq!(m.screen_text(), lines(&["$", "", "", ""]));
        feed(&mut m, 2, b"ls\r\nfile.txt\r\n$ ");
        assert_eq!(m.screen_text(), lines(&["$ ls", "file.txt", "$", ""]));
        assert_eq!(m.tail_text(3), lines(&["file.txt", "$", ""]));
        // The prompt line was edited in place, so this is a rewrite: the region
        // reads correctly and the flag says take the screen. The trailing prompt
        // is not reported — the diff aligns it with the prompt that was there
        // before (identical text), which is why a rewrite is a hint to re-read
        // rather than something to apply.
        let delta = m.delta_since(1);
        assert!(delta.needs_snapshot);
        assert_eq!(delta.lines, lines(&["$ ls", "file.txt"]));
        // A line printed below the prompt is a clean append.
        feed(&mut m, 3, b"\r\nnext");
        let after = m.delta_since(2);
        assert!(!after.needs_snapshot);
        assert_eq!(after.lines, lines(&["next"]));
    }

    #[test]
    fn a_full_screen_app_is_read_as_a_screen_not_a_transcript() {
        let mut m = mirror(20, 3);
        feed(&mut m, 1, b"primary output\r\n");
        // Enter the alt screen and paint a two-row UI.
        feed(&mut m, 2, b"\x1b[?1049h\x1b[Hvim: the screen\r\nrow two");
        assert_eq!(m.screen_text(), lines(&["vim: the screen", "row two", ""]));
        // The flip itself is not an append: the delta says re-read.
        assert!(m.delta_since(1).needs_snapshot);
        // Leaving restores the primary screen for the next read.
        feed(&mut m, 4, b"\x1b[?1049l");
        assert_eq!(m.screen_text()[0], "primary output");
        assert!(
            m.delta_since(2).needs_snapshot,
            "the flip back is a rewrite too"
        );
    }

    /// The screen read is the one that stays correct while an app repaints the
    /// same rows over and over: no delta bookkeeping needed.
    #[test]
    fn repeated_repaints_keep_the_screen_read_accurate() {
        let mut m = mirror(20, 3);
        feed(&mut m, 1, b"\x1b[?1049h\x1b[Hframe 1");
        feed(&mut m, 2, b"\x1b[Hframe 2");
        feed(&mut m, 3, b"\x1b[Hframe 3");
        assert_eq!(m.screen_text(), lines(&["frame 3", "", ""]));
        assert!(m.delta_since(2).needs_snapshot);
    }

    #[test]
    fn a_resize_rewraps_and_is_reported_as_a_reset() {
        let mut m = mirror(20, 3);
        feed(&mut m, 1, b"keepme\r\nsecond");
        assert_eq!(m.screen_text(), lines(&["keepme", "second", ""]));
        m.resize(2, 10, 3);
        assert_eq!(m.cols(), 10);
        let delta = m.delta_since(1);
        assert!(
            delta.needs_snapshot,
            "a resize re-wraps, it does not append"
        );
        assert_eq!(delta.through_seq, 2);
        // Output after the resize diffs cleanly again.
        feed(&mut m, 3, b"\r\nthird");
        let after = m.delta_since(2);
        assert!(!after.needs_snapshot);
        assert_eq!(after.lines, lines(&["third"]));
    }

    #[test]
    fn scrolling_a_full_screen_reports_only_the_new_line() {
        let mut m = mirror(20, 3);
        for seq in 1..=6u64 {
            feed(&mut m, seq, format!("line{seq}\r\n").as_bytes());
        }
        // Each step after the screen filled scrolled the window: the delta must
        // carry the new line only, not the whole screen again.
        let delta = m.delta_since(5);
        assert!(!delta.needs_snapshot);
        assert_eq!(delta.lines, lines(&["line6"]));
        // And the history is still readable as text.
        assert_eq!(m.tail_text(30).first().unwrap(), "line1");
    }

    #[test]
    fn resync_rebaselines_the_journal() {
        let mut m = mirror(20, 3);
        feed(&mut m, 1, b"old content\r\n");
        m.resync(2);
        let delta = m.delta_since(2);
        assert!(delta.lines.is_empty());
        assert!(!delta.truncated);
        feed(&mut m, 3, b"new\r\n");
        assert_eq!(m.delta_since(2).lines, lines(&["new"]));
    }

    #[test]
    fn the_mirror_exposes_the_emulator_for_mode_reads() {
        let mut m = mirror(20, 3);
        feed(&mut m, 1, b"\x1b]0;my title\x07");
        assert_eq!(m.title(), Some("my title"));
        // The BEL that ends an OSC string is a terminator, not a bell.
        assert_eq!(m.seq(), 1);
        assert_eq!(m.emulator().rows(), 3);
        assert_eq!(
            m.emulator().mouse_modes().protocol,
            crate::MouseProtocol::Off
        );
    }

    #[test]
    fn the_mirror_is_send_for_the_engine_session_mutex() {
        fn assert_send<T: Send>() {}
        assert_send::<TerminalMirror>();
        assert_send::<TextJournal>();
    }

    /// The engine's `last_command` read speaks the blocks through
    /// the mirror: one A→B→C→D cycle answers command, cwd, output and a NON-ZERO
    /// exit code, marker-backed.
    #[test]
    fn mirror_blocks_answer_command_cwd_output_and_a_nonzero_exit() {
        let mut m = mirror(40, 6);
        feed(&mut m, 1, b"\x1b]133;A;Cwd=/work\x07user@host $ ");
        feed(&mut m, 2, b"\x1b]133;B\x07false\r\n");
        feed(&mut m, 3, b"\x1b]133;C\x07something failed\r\n");
        feed(
            &mut m,
            4,
            b"\x1b]133;D;1;Cwd=/work\x07\x1b]133;A;Cwd=/work\x07user@host $ ",
        );

        let block = m.last_command_block().expect("one command ran");
        assert_eq!(block.command.as_deref(), Some("false"));
        assert_eq!(block.cwd.as_deref(), Some("/work"));
        assert_eq!(block.exit_code, Some(1), "the non-zero exit survives");
        assert!(block.exit_is_known());
        assert!(block.marker_backed);
        assert_eq!(m.block_lines(&block), lines(&["something failed"]));
        assert!(m.saw_markers());
    }

    /// Without markers there are no blocks: `saw_markers` stays false and
    /// nothing is guessed from the prompt geometry.
    #[test]
    fn mirror_has_no_blocks_when_no_markers_arrive() {
        let mut m = mirror(40, 6);
        feed(&mut m, 1, b"$ sleep 2\r\ndone\r\n");
        assert!(!m.saw_markers());
        assert!(m.command_blocks().is_empty(), "no boundary");
        assert!(m.last_command_block().is_none());
    }
}
