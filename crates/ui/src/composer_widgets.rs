//! Composer widget strip: the Changes / subagents / processes chips that sit
//! above the prompt composer. Read-only views over data the shell already
//! holds (no RPC, no writes to the agent).
//!
//! Pure helpers live here first; the gpui rendering is added in later tasks.

use crate::changes::FileDiff;
use std::time::{Duration, Instant};

/// Visibility of one chip with a linger window: the chip appears the moment
/// there is something to show and survives a short period after it empties,
/// so a command or a diff that flickers does not make the pill strobe.
///
/// Changes uses 600 ms; the processes chip 6 s (a command that just finished
/// stays listed) and `pin` (a failed command) keeps it up regardless.
#[derive(Debug, Clone, Copy)]
pub struct LingerGate {
    shown: bool,
    /// When a shown chip finally hides; `None` while something is live.
    until: Option<Instant>,
    linger: Duration,
}

impl LingerGate {
    pub fn new(linger: Duration) -> Self {
        Self {
            shown: false,
            until: None,
            linger,
        }
    }

    /// Advance the gate: `want` is "there is data right now", `pin` is
    /// "something must stay visible" (a failed command). Returns whether the
    /// chip renders.
    pub fn step(&mut self, want: bool, pin: bool, now: Instant) -> bool {
        if want || pin {
            self.shown = true;
            self.until = None;
        } else if self.shown {
            let until = *self.until.get_or_insert(now + self.linger);
            if now >= until {
                self.shown = false;
                self.until = None;
            }
        }
        self.shown
    }

    /// How long until a showing-but-empty chip hides, so the shell can arm a
    /// one-shot wake and repaint then. `None` when nothing is pending.
    pub fn wake_in(&self, now: Instant) -> Option<Duration> {
        self.until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    /// Chat switch: nothing carries over.
    pub fn reset(&mut self) {
        self.shown = false;
        self.until = None;
    }
}

/// A counter rendered in a chip: plain below a thousand, one decimal above
/// (`15190` → `15.2k`) so the pill stays short.
pub fn format_count(n: u32) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let text = format!("{:.1}k", f64::from(n) / 1000.0);
    text.replace(".0k", "k")
}

/// Files touched, additions and deletions — `None` when the tree is clean
/// (nothing to show) or the diff could not be read.
pub fn summarize_diff(files: &[FileDiff]) -> Option<(usize, u32, u32)> {
    if files.is_empty() {
        return None;
    }
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    Some((files.len(), additions, deletions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn file(additions: u32, deletions: u32) -> FileDiff {
        FileDiff {
            path: "a.rs".into(),
            old_path: None,
            status: crate::changes::FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions,
            deletions,
            max_line: 0,
        }
    }

    #[test]
    fn format_count_keeps_small_numbers() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
    }

    #[test]
    fn format_count_uses_one_decimal_above_a_thousand() {
        assert_eq!(format_count(1000), "1k");
        assert_eq!(format_count(1200), "1.2k");
        assert_eq!(format_count(15190), "15.2k");
    }

    #[test]
    fn summarize_diff_is_none_without_files() {
        assert_eq!(summarize_diff(&[]), None);
    }

    #[test]
    fn summarize_diff_sums_files_and_lines() {
        let files = [file(5, 1), file(0, 2)];
        assert_eq!(summarize_diff(&files), Some((2, 5, 3)));
    }

    fn gate(ms: u64) -> LingerGate {
        LingerGate::new(Duration::from_millis(ms))
    }

    #[test]
    fn gate_shows_immediately_then_hides_after_linger() {
        let now = Instant::now();
        let mut gate = gate(600);
        assert!(gate.step(true, false, now));
        assert!(gate.step(false, false, now));
        assert!(gate.step(false, false, now + Duration::from_millis(599)));
        assert_eq!(
            gate.wake_in(now + Duration::from_millis(599)),
            Some(Duration::from_millis(1))
        );
        assert!(!gate.step(false, false, now + Duration::from_millis(600)));
    }

    #[test]
    fn gate_re_arming_restarts_the_linger() {
        let now = Instant::now();
        let mut gate = gate(600);
        gate.step(true, false, now);
        gate.step(false, false, now + Duration::from_millis(500));
        assert!(gate.step(true, false, now + Duration::from_millis(550)));
        assert_eq!(
            gate.wake_in(now + Duration::from_millis(550)),
            None,
            "live data cancels the pending hide"
        );
        gate.step(false, false, now + Duration::from_millis(600));
        assert!(gate.step(false, false, now + Duration::from_millis(1199)));
        assert!(!gate.step(false, false, now + Duration::from_millis(1200)));
    }

    #[test]
    fn gate_pin_keeps_the_chip_up_without_data() {
        let now = Instant::now();
        let mut gate = gate(600);
        assert!(gate.step(false, true, now));
        assert!(gate.step(false, true, now + Duration::from_secs(60)));
        assert_eq!(gate.wake_in(now + Duration::from_secs(60)), None);
    }

    #[test]
    fn gate_linger_differs_per_gate() {
        let now = Instant::now();
        let mut short = gate(600);
        let mut long = gate(6000);
        short.step(true, false, now);
        long.step(true, false, now);
        short.step(false, false, now);
        long.step(false, false, now);
        assert!(!short.step(false, false, now + Duration::from_millis(600)));
        assert!(long.step(false, false, now + Duration::from_millis(600)));
        assert!(!long.step(false, false, now + Duration::from_millis(6000)));
    }

    #[test]
    fn gate_reset_hides_whatever_it_held() {
        let now = Instant::now();
        let mut gate = gate(600);
        gate.step(true, true, now);
        gate.reset();
        assert!(!gate.step(false, false, now));
    }
}
