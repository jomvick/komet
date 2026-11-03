//! Composer widget strip: the Changes / subagents / processes chips that sit
//! above the prompt composer. Read-only views over data the shell already
//! holds (no RPC, no writes to the agent).
//!
//! Pure helpers live here first; the gpui rendering is added in later tasks.

use crate::changes::FileDiff;
use gpui::SharedString;
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

/// Which composer widget a popup belongs to. The strip shares one popup, so
/// the kind is the popup's payload (pickers' `Popup<PickerKind>` pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    Changes,
    Subagents,
    Processes,
}

/// Everything the strip renders from, gathered once per paint by the shell.
#[derive(Debug, Default)]
pub struct StripInput {
    /// `None` = clean tree or no diff yet: the changes chip stays hidden.
    pub widget_changes: Option<(usize, u32, u32)>,
    pub subagent_rows: Vec<crate::transcript::SubagentRowData>,
    pub process_entries: Vec<crate::transcript::AgentShellEntry>,
    /// The harness declares a subagent surface at all.
    pub subagents_allowed: bool,
    /// The harness streams tool progress at all.
    pub processes_allowed: bool,
}

/// One chip's rendered content, decided by pure code so the layout stays dumb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipContent {
    pub visible: bool,
    pub icon: &'static str,
    pub label: SharedString,
}

/// A chip's label and visibility, from the current data and the harness
/// capabilities. Hidden chips render nothing at all.
pub fn chip_content(kind: WidgetKind, input: &StripInput) -> ChipContent {
    let (visible, icon, label) = match kind {
        WidgetKind::Changes => {
            let Some((files, additions, deletions)) = input.widget_changes else {
                return ChipContent {
                    visible: false,
                    icon: crate::icons::FILE_DIFF,
                    label: SharedString::default(),
                };
            };
            let file_word = if files == 1 { "file" } else { "files" };
            (
                true,
                crate::icons::FILE_DIFF,
                format!("{files} {file_word} +{additions} −{deletions}"),
            )
        }
        WidgetKind::Subagents => {
            let count = input.subagent_rows.len();
            let word = if count == 1 { "agent" } else { "agents" };
            (
                input.subagents_allowed && count > 0,
                crate::icons::WIDGET,
                format!("{count} {word}"),
            )
        }
        WidgetKind::Processes => {
            let count = input.process_entries.len();
            let word = if count == 1 { "process" } else { "processes" };
            (
                input.processes_allowed && count > 0,
                crate::icons::TERMINAL,
                format!("{count} {word}"),
            )
        }
    };
    ChipContent {
        visible,
        icon,
        label: SharedString::from(label),
    }
}

/// Cut in the middle so both the command name and its tail survive:
/// `…` replaces the omitted run, and the result is exactly `max` chars.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max == 0 {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 2;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// A process row: the command, cut for the popup, and the last thing it
/// printed (skipping blank trailing lines). Commands run inside the agent's
/// own shell — this view is read-only and has no stop control.
pub fn process_row_lines(
    entry: &crate::transcript::AgentShellEntry,
) -> (String, Option<SharedString>) {
    let command = truncate_middle(&entry.command, PROCESS_COMMAND_MAX);
    let last = entry
        .output
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| SharedString::from(line.trim_end()));
    (command, last)
}

/// Command column width before middle-truncation (a popup row, not a pager).
const PROCESS_COMMAND_MAX: usize = 44;

/// Timeline state of one process entry, for the row's dot.
pub fn process_tint(
    status: crate::transcript::AgentShellStatus,
    theme: &crate::theme::Theme,
) -> gpui::Hsla {
    use crate::transcript::AgentShellStatus;
    match status {
        AgentShellStatus::Running => theme.busy,
        AgentShellStatus::Done => theme.success,
        AgentShellStatus::Failed => theme.danger,
    }
}

/// Whether the harness's declared capabilities allow this chip at all.
/// Changes reads git, so it is never capability-gated.
pub fn chip_allowed(
    subagents: komet_proto::SubagentSurface,
    tool_progress: komet_proto::ProgressSurface,
    kind: WidgetKind,
) -> bool {
    match kind {
        WidgetKind::Changes => true,
        WidgetKind::Subagents => subagents != komet_proto::SubagentSurface::None,
        WidgetKind::Processes => tool_progress != komet_proto::ProgressSurface::None,
    }
}

/// The transcript's "see the commands" affordance opens the processes popup
/// above the composer instead of a right-pane surface.
pub fn retarget_agent_shell(strip: &mut WidgetStrip) {
    strip.open_popup(WidgetKind::Processes);
}

/// The strip's state: one shared popup plus one visibility gate per chip.
pub struct WidgetStrip {
    pub open: crate::popover::Popup<WidgetKind>,
    pub changes_gate: LingerGate,
    pub subagents_gate: LingerGate,
    pub processes_gate: LingerGate,
    /// Claims the keyboard while a popup is open (Escape closes it).
    pub focus: Option<gpui::FocusHandle>,
}

impl Default for WidgetStrip {
    fn default() -> Self {
        Self::new()
    }
}

impl WidgetStrip {
    pub fn new() -> Self {
        Self {
            open: crate::popover::Popup::default(),
            changes_gate: LingerGate::new(Duration::from_millis(600)),
            subagents_gate: LingerGate::new(Duration::from_secs(6)),
            processes_gate: LingerGate::new(Duration::from_secs(6)),
            focus: None,
        }
    }

    pub fn open_popup(&mut self, kind: WidgetKind) {
        self.open.open(kind);
    }

    /// Chat switch: the popup and every linger belong to the old chat.
    pub fn reset(&mut self) {
        self.open = crate::popover::Popup::default();
        self.changes_gate.reset();
        self.subagents_gate.reset();
        self.processes_gate.reset();
    }

    /// Reset only when the selected chat actually changed.
    pub fn reset_checked(&mut self, new_chat: Option<&str>, old_chat: Option<&str>) {
        if new_chat != old_chat {
            self.reset();
        }
    }

    /// Advance the three gates; returns visibility in render order
    /// (Changes, Subagents, Processes).
    pub fn step(&mut self, input: &StripInput, now: Instant) -> [bool; 3] {
        let failed = input
            .process_entries
            .iter()
            .any(|entry| entry.status == crate::transcript::AgentShellStatus::Failed);
        // An open popup holds its own chip up, so a command that finishes
        // while the user reads its row does not yank the menu away.
        let open = self.open.get().copied();
        [
            self.changes_gate.step(
                input.widget_changes.is_some() || open == Some(WidgetKind::Changes),
                false,
                now,
            ),
            self.subagents_gate.step(
                input.subagents_allowed && !input.subagent_rows.is_empty()
                    || open == Some(WidgetKind::Subagents),
                false,
                now,
            ),
            self.processes_gate.step(
                input.processes_allowed && !input.process_entries.is_empty()
                    || open == Some(WidgetKind::Processes),
                failed,
                now,
            ),
        ]
    }

    /// Soonest gate deadline, for the shell's one-shot wake.
    pub fn wake_in(&self, now: Instant) -> Option<Duration> {
        [
            self.changes_gate.wake_in(now),
            self.subagents_gate.wake_in(now),
            self.processes_gate.wake_in(now),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// (`15190` → `15.2k`) so the pill stays short.
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

/// Why a command row shows no output: the harness's own reporting limit, said
/// plainly instead of leaving a blank row.
pub fn no_output_note(
    status: crate::transcript::AgentShellStatus,
    surface: komet_proto::ProgressSurface,
) -> &'static str {
    use crate::transcript::AgentShellStatus;
    use komet_proto::ProgressSurface;
    match (status, surface) {
        (AgentShellStatus::Running, ProgressSurface::Streaming) => {
            "waiting for the command's output…"
        }
        (AgentShellStatus::Running, _) => "this agent reports nothing while the command runs",
        (_, ProgressSurface::Streaming) => "the command printed nothing",
        (_, ProgressSurface::Final) => {
            "no print carried: this agent settles a command's status, not its output"
        }
        (_, ProgressSurface::None) => "no output transmitted by this agent",
    }
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

    fn subagent_row(id: &str) -> crate::transcript::SubagentRowData {
        crate::transcript::SubagentRowData {
            spawn: crate::subagent::SubagentSpawn {
                parent_tool_use_id: id.into(),
                title: "Agent: scan".into(),
                model: None,
                status: Some(komet_doc::SubagentStatus::Running),
                duration_ms: None,
                tool_count: None,
            },
            live: None,
            target_device_id: None,
        }
    }

    fn process_entry(
        status: crate::transcript::AgentShellStatus,
        output: &[&str],
    ) -> crate::transcript::AgentShellEntry {
        crate::transcript::AgentShellEntry {
            id: "p".into(),
            command: "cargo test".into(),
            status,
            output: output.iter().map(|line| (*line).into()).collect(),
            truncated_by: 0,
            output_live: true,
        }
    }

    fn strip_input() -> StripInput {
        StripInput {
            widget_changes: None,
            subagent_rows: Vec::new(),
            process_entries: Vec::new(),
            subagents_allowed: true,
            processes_allowed: true,
        }
    }

    #[test]
    fn truncate_middle_keeps_short_and_exact_strings() {
        assert_eq!(truncate_middle("cargo test", 44), "cargo test");
        let exact = "x".repeat(44);
        assert_eq!(truncate_middle(&exact, 44), exact);
    }

    #[test]
    fn truncate_middle_cuts_the_middle() {
        let long = format!("{}{}", "a".repeat(50), "b".repeat(50));
        let cut = truncate_middle(&long, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.starts_with("aaaa"));
        assert!(cut.ends_with("bbbbb"));
        assert!(cut.contains('…'));
    }

    #[test]
    fn changes_chip_labels_files_and_lines() {
        let mut input = strip_input();
        input.widget_changes = Some((2, 5, 3));
        let chip = chip_content(WidgetKind::Changes, &input);
        assert!(chip.visible);
        assert_eq!(chip.icon, crate::icons::FILE_DIFF);
        assert_eq!(chip.label.as_ref(), "2 files +5 −3");

        input.widget_changes = Some((1, 3, 1));
        assert_eq!(
            chip_content(WidgetKind::Changes, &input).label.as_ref(),
            "1 file +3 −1"
        );

        input.widget_changes = None;
        assert!(!chip_content(WidgetKind::Changes, &input).visible);
    }

    #[test]
    fn subagent_and_process_chips_count_rows() {
        let mut input = strip_input();
        input.subagent_rows = vec![subagent_row("a")];
        let chip = chip_content(WidgetKind::Subagents, &input);
        assert!(chip.visible);
        assert_eq!(chip.label.as_ref(), "1 agent");

        input.subagent_rows.push(subagent_row("b"));
        input.subagent_rows.push(subagent_row("c"));
        assert_eq!(
            chip_content(WidgetKind::Subagents, &input).label.as_ref(),
            "3 agents"
        );

        let mut input = strip_input();
        input.process_entries = vec![process_entry(
            crate::transcript::AgentShellStatus::Running,
            &[],
        )];
        assert_eq!(
            chip_content(WidgetKind::Processes, &input).label.as_ref(),
            "1 process"
        );
        input.process_entries.push(process_entry(
            crate::transcript::AgentShellStatus::Done,
            &[],
        ));
        assert_eq!(
            chip_content(WidgetKind::Processes, &input).label.as_ref(),
            "2 processes"
        );
    }

    #[test]
    fn capability_gating_hides_a_chip_even_with_data() {
        let mut input = strip_input();
        input.subagent_rows = vec![subagent_row("a")];
        input.process_entries = vec![process_entry(
            crate::transcript::AgentShellStatus::Running,
            &[],
        )];
        input.subagents_allowed = false;
        input.processes_allowed = false;
        assert!(!chip_content(WidgetKind::Subagents, &input).visible);
        assert!(!chip_content(WidgetKind::Processes, &input).visible);
    }

    #[test]
    fn process_rows_keep_the_last_non_blank_output_line() {
        let entry = process_entry(
            crate::transcript::AgentShellStatus::Done,
            &["first", "  ", "last line", ""],
        );
        let (command, last) = process_row_lines(&entry);
        assert_eq!(command, "cargo test");
        assert_eq!(last.as_deref(), Some("last line"));

        let empty = process_entry(crate::transcript::AgentShellStatus::Running, &[]);
        assert_eq!(process_row_lines(&empty).1, None);
    }

    #[test]
    fn strip_gates_chips_and_resets_on_chat_switch() {
        let now = Instant::now();
        let mut strip = WidgetStrip::new();
        let mut input = strip_input();
        input.subagent_rows = vec![subagent_row("a")];
        let visibility = strip.step(&input, now);
        assert!(!visibility[0], "no diff, no changes chip");
        assert!(visibility[1], "subagents chip appears immediately");
        assert!(!visibility[2], "no command, no processes chip");
        assert_eq!(
            strip.wake_in(now),
            None,
            "nothing to hide, nothing to schedule"
        );

        input.subagent_rows.clear();
        assert!(strip.step(&input, now)[1]);
        assert_eq!(
            strip.wake_in(now),
            Some(Duration::from_secs(6)),
            "a settled chip lingers"
        );
        strip.reset();
        assert!(!strip.step(&input, now)[1]);
        assert_eq!(strip.wake_in(now), None);
    }

    #[test]
    fn failed_commands_pin_the_processes_chip() {
        let now = Instant::now();
        let mut strip = WidgetStrip::new();
        let mut input = strip_input();
        input.process_entries = vec![process_entry(
            crate::transcript::AgentShellStatus::Failed,
            &[],
        )];
        assert!(strip.step(&input, now)[2]);
        input.process_entries.clear();
        assert!(strip.step(&input, now)[2], "a failure holds the pill up");
    }
}
