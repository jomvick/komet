//! OSC 133 command boundaries: the marker parser and the block model built from
//! it.
//!
//! Semantic-prompt markers (the OSC 133 family, used by VS Code, WezTerm,
//! iTerm2, contour) stamp *where a command begins and ends* onto the byte
//! stream the shell already produces:
//!
//! | letter | meaning                       | emitted by                 |
//! |--------|-------------------------------|------------------------------|
//! | `A`    | the prompt starts             | `PROMPT_COMMAND` / `precmd`  |
//! | `B`    | the prompt ends, typing begins| end of `PS1` / `fish_prompt` |
//! | `C`    | the command starts executing  | `PS0` / `preexec`            |
//! | `D;n`  | it exited with status `n`     | `PROMPT_COMMAND` / `precmd`  |
//!
//! One parser, two consumers — that is the whole point of putting this here:
//! the [`Emulator`] is shared by the gpui grid (which gets prev/next command
//! navigation for free) and by the engine's text mirror (which gets the command
//! blocks an agent reads through `terminal_read`). Neither consumer re-parses
//! the stream, and neither one can drift from the other.
//!
//! Two rules this module is built around:
//!
//! - **Markers must never touch the screen.** They are read out of the byte
//!   stream *alongside* the ANSI parser, never in place of it: the bytes still
//!   reach `vte`, which ignores an unknown OSC. A mis-parsed marker can at worst
//!   produce a wrong block, never a stray character in the grid (see the
//!   `markers_leave_no_visible_characters` test).
//! - **A terminal with no markers is not a terminal with no commands.** Shells
//!   we do not inject (an exotic one, a `PS1`/`PROMPT_COMMAND` the user
//!   overwrote, PowerShell) still get blocks — synthesized from the *silence*
//!   rule by [`CommandTracker::settle_silence`]. Those blocks are explicitly
//!   `marker_backed == false` and carry no command text and no exit code, so an
//!   agent reads a labelled gap instead of a fabricated answer.

/// Which boundary a marker announces. `Property` is the `133;P;key=value` form
/// (VS Code's convention for carrying the cwd alongside a boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// `A` — the prompt is about to be drawn.
    PromptStart,
    /// `B` — the prompt is drawn; everything after this is typed input.
    PromptEnd,
    /// `C` — the command line was accepted and is executing.
    ExecStart,
    /// `D` — the command finished (with an exit code when the shell knows it).
    ExecDone,
    /// `P` — metadata only, no boundary.
    Property,
}

/// One parsed OSC 133 marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMarker {
    pub kind: MarkerKind,
    /// `D;<exit>`; a signal-killed command reports the shell's 128+n convention
    /// verbatim, and a marker without a numeric field leaves this `None`.
    pub exit_code: Option<i32>,
    /// `Cwd=` property (either the `133;P;Cwd=` form or attached to a letter).
    pub cwd: Option<String>,
    /// A command line carried in the marker itself (`E=`), when the shell sends
    /// one. Otherwise the command text comes out of the grid.
    pub command: Option<String>,
}

/// The grid position a marker arrived at: absolute content line (1-based,
/// monotone while history grows — see [`crate::Emulator::content_line`]) and
/// column. The column decides whether the line the cursor sits on still holds
/// the previous command's output (`col > 0`: a program printed a final line
/// without a newline; `col == 0`: the shell owns the row now).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub line: usize,
    pub col: usize,
}

impl Mark {
    /// The first line *after* whatever this mark's line holds: the exclusive end
    /// of a region that includes this mark's own line when it is already
    /// written on, and the line itself when the cursor sits at its start.
    pub fn after(&self) -> usize {
        if self.col == 0 {
            self.line
        } else {
            self.line + 1
        }
    }
}

/// Where a block's `command` text came from — how much an agent should trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    /// Between the `B` marker's column and the output: exactly what was typed.
    Typed,
    /// The shell put the command line in the marker payload.
    Marker,
    /// No `B` marker: the whole line above the output, prompt included.
    LineWithPrompt,
    /// No markers at all (the silence fallback).
    None,
}

/// Whether a command's boundary is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStatus {
    /// The prompt is showing; nothing has been executed yet.
    Prompt,
    /// Between `C` and `D`: still running.
    Running,
    /// `D` arrived.
    Done,
    /// Closed by the silence fallback, not by a marker: the exit code is
    /// unknown because nothing told us.
    Settled,
}

/// One command: what was typed, where it ran, what it printed, how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandBlock {
    /// 0-based, in order of arrival.
    pub index: usize,
    pub status: BlockStatus,
    pub command: Option<String>,
    pub command_source: CommandSource,
    pub cwd: Option<String>,
    pub exit_code: Option<i32>,
    /// Built from OSC 133 markers (`true`) or synthesized from silence (`false`).
    pub marker_backed: bool,
    /// First / last output batch folded into this block (engine seqs; `0` for a
    /// consumer that does not stamp seqs, i.e. the UI grid).
    pub seq_start: u64,
    pub seq_end: u64,
    /// Absolute content line the block's output starts on. The UI scrolls here.
    pub output_start_line: usize,
    /// Exclusive: the line after the last line the block holds. While a block is
    /// `Running` this is read live ([`crate::Emulator::command_blocks`]).
    pub output_end_line: usize,
    /// The line the `A` marker arrived on (the prompt line), when known.
    pub prompt_line: Option<usize>,
}

impl CommandBlock {
    /// Whether an agent may treat [`Self::exit_code`] as an answer: only a
    /// marker-delivered `D` says the command finished.
    pub fn exit_is_known(&self) -> bool {
        self.exit_code.is_some() && self.marker_backed && self.status == BlockStatus::Done
    }
}

/// The parsed markers in one byte slice, with the offsets the caller needs to
/// keep the ANSI parser in sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedMarker {
    pub marker: CommandMarker,
    /// Index just past the marker's terminator in the slice that was scanned.
    /// The caller feeds bytes up to here to the ANSI parser before acting on the
    /// marker, so the grid state it reads is the state the shell was in.
    pub end: usize,
}

/// The result of [`scan_markers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub markers: Vec<ScannedMarker>,
    /// How much of the scanned slice is safe to hand to the ANSI parser now.
    /// Everything from here on is an unterminated OSC string (or a trailing
    /// lone `ESC`) the caller must hold back and re-scan with the next batch.
    pub consumable: usize,
}

/// An OSC string longer than this is treated as noise, not a marker: a shell
/// that never terminates, or a program spamming OSC. The bytes still reach the
/// ANSI parser, so nothing is lost from the screen.
pub const MAX_OSC_BYTES: usize = 4096;

/// Find every OSC 133 marker in `bytes`.
///
/// Deliberately stateless: the caller (`Emulator::feed`) keeps the few held
/// bytes itself and re-scans them with the next batch, which is what makes a
/// marker split across two PTY reads work without the two parsers being able to
/// disagree about where they are in the stream.
pub fn scan_markers(bytes: &[u8]) -> Scan {
    let mut markers = Vec::new();
    // No OSC is open until one says so; the scan only ever stops at the *first*
    // byte it cannot classify, so everything before it is handed back as
    // consumable.
    let mut consumable = bytes.len();
    let mut i = 0usize;
    'scan: while i < bytes.len() {
        while i < bytes.len() && bytes[i] != 0x1b {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let Some(&second) = bytes.get(i + 1) else {
            // A trailing lone `ESC`: it may be the start of `ESC ]`, so it and
            // nothing before it has to wait for the next batch.
            consumable = i;
            break;
        };
        if second != b']' {
            // Not an OSC: the ANSI parser's business, and it stays consumable.
            i += 1;
            continue;
        }
        let start = i;
        let body_start = i + 2;
        let mut j = body_start;
        while j < bytes.len() {
            if j - body_start > MAX_OSC_BYTES {
                // An OSC that never ends (a program spamming escapes, a broken
                // shell): abandon it where it stands and keep scanning, so the
                // stream is never parked behind it.
                i = j;
                continue 'scan;
            }
            match bytes[j] {
                0x07 => {
                    // BEL terminator.
                    if let Some(marker) = parse_osc133(&bytes[body_start..j]) {
                        markers.push(ScannedMarker { marker, end: j + 1 });
                    }
                    i = j + 1;
                    continue 'scan;
                }
                0x1b if bytes.get(j + 1) == Some(&b'\\') => {
                    // ST terminator.
                    if let Some(marker) = parse_osc133(&bytes[body_start..j]) {
                        markers.push(ScannedMarker { marker, end: j + 2 });
                    }
                    i = j + 2;
                    continue 'scan;
                }
                0x1b => {
                    // An `ESC` that is not ST means this OSC string was never
                    // terminated. Drop it and look at that `ESC` as the start of
                    // whatever sequence it begins.
                    i = j;
                    continue 'scan;
                }
                _ => j += 1,
            }
        }
        // Open at the end of the slice: hold the whole OSC back, from its `ESC`.
        consumable = start;
        break;
    }
    Scan {
        markers,
        consumable,
    }
}

/// Parse the body of an OSC string (`133;A;Cwd=/tmp`) into a marker, or `None`
/// when it is some other OSC (a title, a color, an escape we do not model).
pub fn parse_osc133(body: &[u8]) -> Option<CommandMarker> {
    let text = String::from_utf8_lossy(body);
    let mut fields = text.split(';');
    if fields.next()? != "133" {
        return None;
    }
    let sub = fields.next()?.trim();
    let kind = match sub {
        "A" => MarkerKind::PromptStart,
        "B" => MarkerKind::PromptEnd,
        "C" => MarkerKind::ExecStart,
        "D" => MarkerKind::ExecDone,
        "P" => MarkerKind::Property,
        _ => return None,
    };
    let mut marker = CommandMarker {
        kind,
        exit_code: None,
        cwd: None,
        command: None,
    };
    for field in fields {
        let field = field.trim();
        if let Some(value) = field
            .strip_prefix("Cwd=")
            .or_else(|| field.strip_prefix("cwd="))
        {
            marker.cwd = Some(strip_file_url(value));
        } else if let Some(value) = field
            .strip_prefix("E=")
            .or_else(|| field.strip_prefix("e="))
        {
            marker.command = Some(decode_percent(value));
        } else if kind == MarkerKind::ExecDone {
            // `133;D;0` — a bare numeric field is the exit status. Anything else
            // (empty, `-`, a non-numeric payload) stays unknown rather than
            // becoming a plausible-looking 0.
            if let Ok(code) = field.parse::<i32>() {
                marker.exit_code = Some(code);
            }
        }
    }
    Some(marker)
}

/// `file:///tmp/x` and `/tmp/x` both arrive from real shells.
fn strip_file_url(value: &str) -> String {
    let decoded = decode_percent(value);
    if let Some(path) = decoded.strip_prefix("file://") {
        return path.to_string();
    }
    decoded
}

/// Percent-decoding for the `Cwd=`/`E=` payloads (`%25`, `%20`, `%3B`…). A
/// malformed escape is passed through verbatim instead of dropping text.
fn decode_percent(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16).ok()
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The blocks an OSC 133 stream describes. Pure bookkeeping over [`Mark`]s: the
/// grid text (the command line, the output region) is resolved by the caller,
/// which is the only thing that can read it.
#[derive(Debug, Default)]
pub struct CommandTracker {
    blocks: Vec<CommandBlock>,
    open: Option<CommandBlock>,
    /// Did a single valid marker ever arrive? The silence fallback only speaks
    /// when markers are absent — with markers present, a missing `D` means the
    /// command is genuinely still running.
    saw_markers: bool,
    /// `B` of the prompt currently being typed.
    prompt_end: Option<Mark>,
    /// Metadata awaiting the next boundary.
    pending_cwd: Option<String>,
    pending_command: Option<String>,
    /// The line the last silence-fallback block ended on.
    settled_line: usize,
}

impl CommandTracker {
    pub fn blocks(&self) -> &[CommandBlock] {
        &self.blocks
    }

    pub fn saw_markers(&self) -> bool {
        self.saw_markers
    }

    pub fn prompt_end_mark(&self) -> Option<Mark> {
        self.prompt_end
    }

    /// The open block, if a command is between `C` and `D` (or a prompt is
    /// showing).
    pub fn open(&self) -> Option<&CommandBlock> {
        self.open.as_ref()
    }

    /// Where the silence fallback resumes: blocks are synthesized after this
    /// line, so a second call only describes genuinely new output.
    pub fn settled_line(&self) -> usize {
        self.settled_line
    }

    /// `133;P;Cwd=` (and the payload-carrying forms) land here, then attach to
    /// the next boundary.
    pub fn note_property(&mut self, marker: &CommandMarker) {
        self.saw_markers = true;
        if marker.cwd.is_some() {
            self.pending_cwd = marker.cwd.clone();
        }
        if marker.command.is_some() {
            self.pending_command = marker.command.clone();
        }
    }

    /// `A`: the previous command is over (its `D` may have been lost — a user
    /// who `Ctrl-C`'d the shell mid-prompt, a `PROMPT_COMMAND` that runs ours
    /// then dies), so close it, and open the prompt block.
    pub fn note_prompt_start(&mut self, mark: Mark, cwd: Option<String>) {
        self.saw_markers = true;
        self.close_mark(Some(mark.line), None);
        let cwd = cwd.or_else(|| self.pending_cwd.take());
        self.prompt_end = None;
        self.pending_command = None;
        self.open = Some(CommandBlock {
            index: self.blocks.len(),
            status: BlockStatus::Prompt,
            command: None,
            command_source: CommandSource::None,
            cwd,
            exit_code: None,
            marker_backed: true,
            seq_start: 0,
            seq_end: 0,
            output_start_line: mark.line,
            output_end_line: mark.line,
            prompt_line: Some(mark.line),
        });
    }

    /// `B`: everything from this column of this line to the output is input.
    pub fn note_prompt_end(&mut self, mark: Mark) {
        self.saw_markers = true;
        self.prompt_end = Some(mark);
    }

    /// `C`: the typed line is over and the output starts here.
    pub fn note_exec_start(
        &mut self,
        mark: Mark,
        command: Option<String>,
        source: CommandSource,
        cwd: Option<String>,
    ) {
        self.saw_markers = true;
        let cwd = cwd
            .or(self.pending_cwd.take())
            .or_else(|| self.open.as_ref().and_then(|block| block.cwd.clone()));
        let command = command.or_else(|| self.pending_command.take());
        let source = if command.is_some() {
            match source {
                CommandSource::None => CommandSource::Marker,
                other => other,
            }
        } else {
            CommandSource::None
        };
        let open = self.open.get_or_insert(CommandBlock {
            index: self.blocks.len(),
            status: BlockStatus::Prompt,
            command: None,
            command_source: CommandSource::None,
            cwd: None,
            exit_code: None,
            marker_backed: true,
            seq_start: 0,
            seq_end: 0,
            output_start_line: mark.line,
            output_end_line: mark.line,
            prompt_line: None,
        });
        if open.command.is_none() {
            open.command = command;
            open.command_source = source;
        }
        if open.cwd.is_none() {
            open.cwd = cwd;
        }
        open.status = BlockStatus::Running;
        open.marker_backed = true;
        open.output_start_line = mark.after();
        open.prompt_line = open.prompt_line.or(Some(mark.line));
        self.prompt_end = None;
    }

    /// `D`: the command is finished with this status.
    pub fn note_exec_done(&mut self, mark: Mark, exit_code: Option<i32>, cwd: Option<String>) {
        self.saw_markers = true;
        if let Some(block) = self.open.as_mut() {
            block.status = BlockStatus::Done;
            block.exit_code = exit_code;
            block.output_end_line = block.output_end_line.max(mark.after());
            if cwd.is_some() {
                block.cwd = cwd;
            } else if block.cwd.is_none() {
                block.cwd = self.pending_cwd.take();
            }
        }
        self.close_mark(Some(mark.after()), exit_code);
        self.prompt_end = None;
    }

    /// Push the open block into the list. `end_line` is the exclusive end of its
    /// output region when the caller knows one.
    fn close_mark(&mut self, end_line: Option<usize>, _exit: Option<i32>) {
        if let Some(mut block) = self.open.take() {
            if let Some(end) = end_line {
                block.output_end_line = block.output_end_line.max(end);
            }
            if block.output_end_line < block.output_start_line {
                block.output_end_line = block.output_start_line;
            }
            // A block closed without ever seeing output still describes a
            // command, so keep it: `ls` printing nothing is an answer.
            if block.status == BlockStatus::Running {
                // The shell is back at a prompt, so whatever was running is not
                // running any more — even though its `D` never arrived (a
                // Ctrl-C at the prompt, a `PROMPT_COMMAND` that died after our
                // own part). The command is over; its status code is unknown.
                block.status = BlockStatus::Done;
            }
            self.blocks.push(block);
        }
    }

    /// Fold the engine's output seq into the blocks made from this batch:
    /// a block with no `seq_start` starts here, and a block with no `seq_end`
    /// (created and closed inside this one batch) ends here. An open block keeps
    /// tracking the newest batch it covers; a closed one is finished, so its
    /// range never slides forward with later output.
    pub fn attach_seq(&mut self, seq: u64) {
        for block in self.blocks.iter_mut() {
            if block.seq_start == 0 {
                block.seq_start = seq;
            }
            if block.seq_end == 0 {
                block.seq_end = seq;
            }
        }
        if let Some(block) = self.open.as_mut() {
            if block.seq_start == 0 {
                block.seq_start = seq;
            }
            block.seq_end = seq;
        }
    }

    /// Live view: the open block's output region extended to the current line.
    pub(crate) fn live(&self, current: Mark) -> Vec<CommandBlock> {
        let mut blocks = self.blocks.clone();
        if let Some(open) = self.open.as_ref() {
            let mut open = open.clone();
            open.output_end_line = open.output_start_line.max(current.after());
            blocks.push(open);
        }
        blocks
    }

    /// The fallback for a terminal that emits no markers: close everything
    /// printed since the last settlement as one *labelled-unknown* block. No
    /// command text, no cwd, no exit code — the block says what it knows and
    /// `marker_backed == false` says why.
    ///
    /// Returns `true` when a block was created (so a caller polling on an idle
    /// timer does not synthesize empty blocks forever).
    pub fn settle_silence(&mut self, current: Mark) -> bool {
        if self.saw_markers {
            return false;
        }
        if let Some(open) = self.open.as_mut() {
            open.output_end_line = current.line;
        }
        let start = self.settled_line.max(1);
        // Nothing new since the last settlement: no block.
        if current.after() <= start {
            self.settled_line = current.line;
            return false;
        }
        let block = CommandBlock {
            index: self.blocks.len(),
            status: BlockStatus::Settled,
            command: None,
            command_source: CommandSource::None,
            cwd: None,
            exit_code: None,
            marker_backed: false,
            seq_start: 0,
            seq_end: 0,
            output_start_line: start,
            output_end_line: current.after(),
            prompt_line: None,
        };
        self.blocks.push(block);
        self.settled_line = current.after();
        true
    }
}

/// The nearest block whose output starts at or before `line` (backwards
/// navigation): the last such block.
pub fn block_before(blocks: &[CommandBlock], line: usize) -> Option<&CommandBlock> {
    blocks.iter().rfind(|block| block.output_start_line <= line)
}

/// The nearest block whose output starts after `line` (forwards navigation).
pub fn block_after(blocks: &[CommandBlock], line: usize) -> Option<&CommandBlock> {
    blocks.iter().find(|block| block.output_start_line > line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> Vec<CommandMarker> {
        scan_markers(text.as_bytes())
            .markers
            .into_iter()
            .map(|found| found.marker)
            .collect()
    }

    fn bel(body: &str) -> String {
        format!("\x1b]{body}\x07")
    }

    fn st(body: &str) -> String {
        format!("\x1b]{body}\x1b\\")
    }

    // ---- the parser ----

    #[test]
    fn parses_each_boundary_letter() {
        assert_eq!(
            scan(&bel("133;A")),
            vec![CommandMarker {
                kind: MarkerKind::PromptStart,
                exit_code: None,
                cwd: None,
                command: None,
            }]
        );
        assert_eq!(scan(&bel("133;B"))[0].kind, MarkerKind::PromptEnd);
        assert_eq!(scan(&bel("133;C"))[0].kind, MarkerKind::ExecStart);
        assert_eq!(scan(&bel("133;D;0"))[0].kind, MarkerKind::ExecDone);
        assert_eq!(scan(&bel("133;D;1")).first().unwrap().exit_code, Some(1));
        assert_eq!(
            scan(&bel("133;D;130")).first().unwrap().exit_code,
            Some(130),
            "a signal-killed command's 128+n status is a real exit code"
        );
    }

    #[test]
    fn accepts_both_terminators_and_reports_the_end_offset() {
        let found = scan_markers(st("133;A").as_bytes());
        assert_eq!(found.markers.len(), 1);
        assert_eq!(found.consumable, st("133;A").len());
        let found = scan_markers(bel("133;A").as_bytes());
        assert_eq!(found.markers.len(), 1);
        assert_eq!(found.consumable, bel("133;A").len());
        // The offset is where the caller must split to see the grid state at the
        // marker: text before it is already parsed.
        let input = format!("before{}after", bel("133;D;2"));
        let found = scan_markers(input.as_bytes());
        assert_eq!(found.markers[0].end, "before".len() + bel("133;D;2").len());
    }

    #[test]
    fn carries_cwd_and_command_properties() {
        let marker = scan(&bel("133;A;Cwd=/tmp/one")).remove(0);
        assert_eq!(marker.cwd.as_deref(), Some("/tmp/one"));
        let marker = scan(&st("133;P;Cwd=file%3A%2F%2F%2Ftmp%2Ftwo")).remove(0);
        assert_eq!(marker.kind, MarkerKind::Property);
        assert_eq!(marker.cwd.as_deref(), Some("/tmp/two"));
        let marker = scan(&bel("133;C;E=ls%20-la%3Bx")).remove(0);
        assert_eq!(marker.command.as_deref(), Some("ls -la;x"));
    }

    /// Other OSC strings (titles, colors, links) are nobody's business but the
    /// title parser's — they must not become boundaries, and they must not stop
    /// a marker that follows them in the same batch.
    #[test]
    fn ignores_other_osc_strings() {
        let input = format!("{}{}\x1b]0;a title\x07", bel("133;A"), bel("133;B"));
        let kinds: Vec<_> = scan(&input).into_iter().map(|m| m.kind).collect();
        assert_eq!(kinds, vec![MarkerKind::PromptStart, MarkerKind::PromptEnd]);
    }

    #[test]
    fn unknown_or_malformed_133_payloads_are_not_boundaries() {
        // A subcode we do not know, and a `D` whose status is not a number: both
        // stay unknown rather than becoming a plausible 0.
        assert!(scan(&bel("133;Z")).is_empty());
        assert!(scan(&bel("133")).is_empty());
        assert_eq!(scan(&bel("133;D")).first().unwrap().exit_code, None);
        assert_eq!(scan(&bel("133;D;nan")).first().unwrap().exit_code, None);
    }

    /// PTY reads cut anywhere. A marker split across two batches must still be
    /// one marker — the caller holds back the unterminated part.
    #[test]
    fn a_marker_split_across_reads_is_still_found() {
        let whole = format!("{}\r\nmade7 one", bel("133;C"));
        let bytes = whole.as_bytes();
        // Cut in the middle of the OSC body.
        let cut = whole.find("133;C").unwrap() + 2;
        let first = scan_markers(&bytes[..cut]);
        assert!(
            first.markers.is_empty(),
            "not terminated yet: {:?}",
            first.markers
        );
        assert_eq!(
            first.consumable,
            whole.find('\x1b').unwrap(),
            "the open OSC is held back, its ESC and before-text are not"
        );
        let mut held = bytes[first.consumable..cut].to_vec();
        held.extend_from_slice(&bytes[cut..]);
        let second = scan_markers(&held);
        assert_eq!(second.markers.len(), 1);
        assert_eq!(second.markers[0].marker.kind, MarkerKind::ExecStart);
    }

    #[test]
    fn a_marker_split_between_esc_and_bracket_is_still_found() {
        // The caller's protocol: feed `..consumable`, hold the rest, prepend it
        // to the next batch. A held lone `ESC` must therefore be re-scanned with
        // the bytes that follow it.
        let whole = bel("133;A");
        let bytes = whole.as_bytes();
        let first = scan_markers(&bytes[..1]);
        assert!(first.markers.is_empty());
        assert_eq!(first.consumable, 0, "the lone ESC cannot be classified");
        let mut held = bytes[..1].to_vec();
        held.extend_from_slice(&bytes[1..]);
        let second = scan_markers(&held);
        assert_eq!(second.markers.len(), 1);
        assert_eq!(second.markers[0].marker.kind, MarkerKind::PromptStart);
    }

    #[test]
    fn an_unterminated_osc_does_not_stall_the_stream() {
        // No terminator, and nothing after it: held (the caller will re-scan it
        // with the next batch, and the ANSI parser eventually gets the bytes).
        let open = scan_markers(b"\x1b]133;A");
        assert!(open.markers.is_empty());
        assert_eq!(open.consumable, 0);
        // An OSC broken by the next escape sequence: abandoned, and its bytes
        // are handed to the ANSI parser instead of parked forever.
        let broken = b"\x1b]133;A\x1b[1mhi";
        let found = scan_markers(broken);
        assert!(found.markers.is_empty());
        assert_eq!(found.consumable, broken.len());
    }

    /// A program spamming an endless OSC must not park the whole stream in the
    /// held buffer forever.
    #[test]
    fn an_over_long_osc_is_abandoned_not_held() {
        let mut spam = b"\x1b]133;A".to_vec();
        spam.extend(std::iter::repeat_n(b'x', MAX_OSC_BYTES + 10));
        let found = scan_markers(&spam);
        assert!(found.markers.is_empty());
        assert_eq!(found.consumable, spam.len());
    }

    #[test]
    fn plain_text_produces_no_markers() {
        let input: &[u8] = b"$ ls\r\nfile.txt\r\n";
        let found = scan_markers(input);
        assert!(found.markers.is_empty());
        assert_eq!(
            found.consumable,
            input.len(),
            "all of it is the ANSI parser's"
        );
    }

    // ---- the block model ----

    /// The acceptance this whole step exists for: one marker sequence gives the
    /// command, the cwd, the output region and the exit code — including a
    /// non-zero one.
    #[test]
    fn markers_describe_a_command_with_its_nonzero_exit() {
        let mut tracker = CommandTracker::default();
        tracker.note_prompt_start(Mark { line: 3, col: 0 }, Some("/srv/app".into()));
        tracker.note_prompt_end(Mark { line: 3, col: 12 });
        tracker.note_exec_start(
            Mark { line: 3, col: 20 },
            Some("make deploy".into()),
            CommandSource::Typed,
            None,
        );
        tracker.note_exec_done(Mark { line: 9, col: 0 }, Some(2), None);
        let blocks = tracker.blocks();
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.command.as_deref(), Some("make deploy"));
        assert_eq!(block.cwd.as_deref(), Some("/srv/app"));
        assert_eq!(block.exit_code, Some(2));
        assert!(block.exit_is_known());
        assert_eq!(block.status, BlockStatus::Done);
        assert_eq!(
            (block.output_start_line, block.output_end_line),
            (4, 9),
            "the output is the lines the command wrote, not the prompt"
        );
        assert!(block.marker_backed);
    }

    /// `col > 0` at `D` is the "the program's last line had no newline" case:
    /// that line belongs to the command's output, not to the prompt.
    #[test]
    fn output_region_includes_a_line_written_without_a_newline() {
        let mut tracker = CommandTracker::default();
        tracker.note_prompt_start(Mark { line: 5, col: 0 }, None);
        tracker.note_exec_start(Mark { line: 5, col: 8 }, None, CommandSource::None, None);
        tracker.note_exec_done(Mark { line: 7, col: 3 }, Some(0), None);
        let block = &tracker.blocks()[0];
        assert_eq!(block.output_start_line, 6);
        assert_eq!(block.output_end_line, 8);
        // No command text was recoverable (no `B`), and the block says so.
        assert!(block.command.is_none());
        assert_eq!(block.command_source, CommandSource::None);
    }

    /// A user who `Ctrl-C`s the prompt leaves no `D`. The next `A` must close the
    /// dangling block instead of merging two commands into one.
    #[test]
    fn a_lost_done_marker_is_closed_by_the_next_prompt() {
        let mut tracker = CommandTracker::default();
        tracker.note_prompt_start(Mark { line: 1, col: 0 }, None);
        tracker.note_exec_start(
            Mark { line: 1, col: 6 },
            Some("sleep 9".into()),
            CommandSource::Typed,
            None,
        );
        assert!(tracker.open().is_some());
        tracker.note_prompt_start(Mark { line: 6, col: 0 }, None);
        let blocks = tracker.blocks();
        assert_eq!(blocks.len(), 1, "the interrupted command became a block");
        assert_eq!(blocks[0].exit_code, None);
        assert!(!blocks[0].exit_is_known());
        assert_eq!(
            blocks[0].status,
            BlockStatus::Done,
            "the interrupted command is over — the shell is back at a prompt"
        );
        assert_eq!(
            tracker.open().expect("the new prompt is open").status,
            BlockStatus::Prompt,
            "and the next prompt starts a fresh block instead of extending it"
        );
        assert_eq!(blocks[0].output_end_line, 6);
    }

    #[test]
    fn seqs_are_folded_in_at_the_batch_that_made_the_block() {
        let mut tracker = CommandTracker::default();
        tracker.note_prompt_start(Mark { line: 1, col: 0 }, None);
        tracker.attach_seq(4);
        tracker.note_exec_start(Mark { line: 1, col: 4 }, None, CommandSource::None, None);
        tracker.note_exec_done(Mark { line: 3, col: 0 }, Some(0), None);
        tracker.attach_seq(7);
        let block = &tracker.blocks()[0];
        assert_eq!((block.seq_start, block.seq_end), (4, 4));
        assert_eq!(
            block.seq_end, 4,
            "a finished block's range is fixed: it cannot cover output that came \
             after its own `D`, so a later seq must not stretch it"
        );
        // An unfinished block does keep tracking the newest batch.
        tracker.note_prompt_start(Mark { line: 5, col: 0 }, None);
        tracker.attach_seq(8);
        tracker.note_exec_start(Mark { line: 5, col: 2 }, None, CommandSource::None, None);
        tracker.attach_seq(11);
        let open = tracker.open().expect("running block");
        assert_eq!((open.seq_start, open.seq_end), (8, 11));
    }

    #[test]
    fn property_cwd_attaches_to_the_next_boundary() {
        let mut tracker = CommandTracker::default();
        tracker.note_property(&CommandMarker {
            kind: MarkerKind::Property,
            exit_code: None,
            cwd: Some("/tmp/prop".into()),
            command: None,
        });
        tracker.note_prompt_start(Mark { line: 2, col: 0 }, None);
        tracker.note_exec_start(Mark { line: 2, col: 6 }, None, CommandSource::None, None);
        tracker.note_exec_done(Mark { line: 4, col: 0 }, Some(0), None);
        assert_eq!(tracker.blocks()[0].cwd.as_deref(), Some("/tmp/prop"));
    }

    #[test]
    fn live_view_extends_a_running_block_to_the_current_line() {
        let mut tracker = CommandTracker::default();
        tracker.note_prompt_start(Mark { line: 1, col: 0 }, None);
        tracker.note_exec_start(Mark { line: 1, col: 5 }, None, CommandSource::None, None);
        let live = tracker.live(Mark { line: 40, col: 0 });
        assert_eq!(live.len(), 1, "the open block is included");
        assert_eq!(live[0].status, BlockStatus::Running);
        assert_eq!(live[0].output_end_line, 40);
        assert_eq!(tracker.blocks().len(), 0, "the stored list is untouched");
    }

    // ---- the silence fallback ----

    /// A terminal with no markers still gets blocks — labelled as what they are.
    #[test]
    fn silence_settles_unmarked_output() {
        let mut tracker = CommandTracker::default();
        // Nothing on screen yet: the fallback invents no block.
        assert!(!tracker.settle_silence(Mark { line: 1, col: 0 }));
        // Output arrived: one labelled-unknown block over the lines it filled.
        assert!(tracker.settle_silence(Mark { line: 12, col: 0 }));
        let block = &tracker.blocks()[0];
        assert!(!block.marker_backed);
        assert_eq!(block.status, BlockStatus::Settled);
        assert_eq!(block.command_source, CommandSource::None);
        assert_eq!((block.output_start_line, block.output_end_line), (1, 12));
        assert!(block.command.is_none(), "the command text is not guessed");
        assert!(block.exit_code.is_none());
        // A second settle with no new output makes no second block.
        assert!(!tracker.settle_silence(Mark { line: 12, col: 0 }));
        assert!(tracker.settle_silence(Mark { line: 20, col: 0 }));
        assert_eq!(tracker.blocks().len(), 2);
        assert_eq!(tracker.blocks()[1].output_start_line, 12);
    }

    /// Once markers arrive, silence must stop inventing blocks: a running command
    /// is running, and the tracker knows the difference.
    #[test]
    fn silence_never_fabricates_a_block_for_a_marked_terminal() {
        let mut tracker = CommandTracker::default();
        tracker.note_prompt_start(Mark { line: 1, col: 0 }, None);
        tracker.note_exec_start(
            Mark { line: 1, col: 4 },
            Some("sleep 30".into()),
            CommandSource::Typed,
            None,
        );
        assert!(!tracker.settle_silence(Mark { line: 30, col: 0 }));
        assert!(tracker.blocks().is_empty());
        assert_eq!(tracker.open().unwrap().status, BlockStatus::Running);
    }

    // ---- navigation ----

    #[test]
    fn navigation_walks_the_blocks_without_skipping_or_repeating() {
        let mut tracker = CommandTracker::default();
        for (prompt, done) in [(1usize, 4usize), (5, 8), (9, 12)] {
            tracker.note_prompt_start(
                Mark {
                    line: prompt,
                    col: 0,
                },
                None,
            );
            tracker.note_exec_start(
                Mark {
                    line: prompt,
                    col: 2,
                },
                None,
                CommandSource::None,
                None,
            );
            tracker.note_exec_done(Mark { line: done, col: 0 }, Some(0), None);
        }
        let blocks = tracker.blocks();
        // Standing on line 13 (below everything): back walks 3, 2, 1, then stays.
        let mut at = 13;
        let mut walked = Vec::new();
        for _ in 0..4 {
            match block_before(blocks, at) {
                Some(block) => {
                    walked.push(block.index);
                    at = block.output_start_line.saturating_sub(1);
                }
                None => break,
            }
        }
        assert_eq!(walked, vec![2, 1, 0]);
        // And forward from the top returns each one once.
        let mut at = 0;
        let mut walked = Vec::new();
        while let Some(block) = block_after(blocks, at) {
            walked.push(block.index);
            at = block.output_start_line;
        }
        assert_eq!(walked, vec![0, 1, 2]);
        assert!(block_after(blocks, 12).is_none());
        assert!(block_before(blocks, 0).is_none());
    }

    #[test]
    fn decode_percent_leaves_malformed_escapes_alone() {
        assert_eq!(decode_percent("%2Ftmp"), "/tmp");
        assert_eq!(decode_percent("100%"), "100%");
        assert_eq!(decode_percent("%zz"), "%zz");
    }
}
