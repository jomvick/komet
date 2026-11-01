//! Terminals — PTY sessions owned by this device (feature-inventory §3.4; port of
//! komet's `terminals.ts` over `portable-pty`).
//!
//! - `open` spawns the user's login shell in the chat's cwd; `subscribe` replays a
//!   bounded 1MB window (resumable via `afterSeq`) then tails live output, batched
//!   at [`TERMINAL_OUTPUT_BATCH_MS`]; data rides base64 (PTY bytes ≠ UTF-8).
//! - Live shells survive subscriber detach — a detached session is the user's
//!   running process, kept until its tab is explicitly closed or the engine exits.
//!   Only EXITED sessions expire (30min TTL on their inert replay buffers), and
//!   [`MAX_TERMINALS`] bounds leakage from renderers that lost their tab state.
//! - Ownership: M5 is single-user local — every IPC/relay caller is the device
//!   owner, so the per-user owner re-checks from komet's Router land with real
//!   multi-account auth in M6.
//! - **Text mirror** (always on, every terminal): the same bytes that ride the
//!   replay window are folded into a `komet_term::TerminalMirror`, so the engine
//!   knows what the terminal *displays* and not only what it printed: the
//!   current screen in clear text, the last N lines, what appeared since a given
//!   `seq`, and the last OSC 133 command block.
//! - **Agents read, never write**: the MCP surface lists and reads every
//!   terminal ([`Terminals::screen_text`], [`Terminals::tail_text`],
//!   [`Terminals::text_delta`], [`Terminals::last_command`]); nothing reachable by
//!   an agent opens, closes, resizes or types into one. Input comes from the UI
//!   (`WriteTerminal`) only.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use komet_doc::TERMINAL_OUTPUT_BATCH_MS;
use komet_term::{Emulator, TerminalMirror, TextDelta};

// The read replies and the list row are transport types (komet_proto, shared
// with the RPC layer and the UI). Re-exported here so `crate::terminals` stays
// the engine's vocabulary for them (see the re-export in `lib.rs`).
use komet_proto::{TerminalCommand, TerminalEvent, TerminalSession};
pub use komet_proto::{TerminalDelta, TerminalInfo, TerminalScreen, TerminalTail};
use komet_term::{BlockStatus, CommandBlock};

use crate::{EngineError, new_id};

const MAX_TERMINALS: usize = 32;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_REPLAY_BYTES: usize = 1024 * 1024;
const EXITED_TTL: Duration = Duration::from_secs(30 * 60);
const REAPER_INTERVAL: Duration = Duration::from_secs(60);

/// Scrollback kept inside a terminal's mirror. The UI grid keeps its own (user
/// setting, 10k default); the engine only needs enough history to answer a tail
/// read, and each mirrored line costs a row of cells — so this is deliberately
/// smaller. 1000 lines is a few MB per terminal at a wide column count.
const TRACKED_SCROLLBACK_LINES: usize = 1_000;

/// The engine's mirror of one terminal: the shared emulator core plus the
/// seq-stamped text journal. It answers the three reads an agent needs —
/// [`TerminalMirror::screen_text`] (what is displayed), [`TerminalMirror::tail_text`]
/// (what scrolled past) and [`TerminalMirror::delta_since`] (what appeared since
/// the last read) — and nothing else. Emulation policy lives in `komet-term`;
/// this type only owns the grid size the engine picked and the engine's answers.
#[derive(Debug)]
struct TerminalTrack {
    mirror: TerminalMirror,
}

impl TerminalTrack {
    fn new(cols: u16, rows: u16) -> Self {
        let emulator = Emulator::with_scrollback(cols, rows, TRACKED_SCROLLBACK_LINES);
        Self {
            mirror: TerminalMirror::new(emulator),
        }
    }

    /// Fold one output batch. The emulator's PTY write-back (DSR/DA query
    /// answers) is deliberately dropped: the engine mirrors the stream, it does
    /// not drive it, and answering from here would double every reply the UI's
    /// own grid already sends.
    fn feed(&mut self, seq: u64, bytes: &[u8]) {
        let _responses = self.mirror.feed(seq, bytes);
    }

    /// A resize re-wraps the grid: no delta across it can be an append, so the
    /// journal marks the step as needing a fresh screen read. It is stamped at
    /// the newest *output* seq (a resize is not output and mints no seq), which
    /// is what makes [`Terminals::text_delta`] report `needs_snapshot` to a
    /// caller that is otherwise caught up.
    fn resize(&mut self, seq: u64, cols: u16, rows: u16) {
        self.mirror.resize(seq, cols, rows);
    }
}

/// Stamp a [`TextDelta`] answer from the mirror with the terminal's id (the
/// proto `TerminalDelta` is a transport type, so it carries no `From` impl for
/// the engine-private `komet-term` journal step).
fn terminal_delta(terminal_id: &str, delta: TextDelta) -> TerminalDelta {
    TerminalDelta {
        terminal_id: terminal_id.to_string(),
        from_seq: delta.from_seq,
        through_seq: delta.through_seq,
        lines: delta.lines,
        needs_snapshot: delta.needs_snapshot,
        truncated: delta.truncated,
    }
}

/// The wire word for a block's boundary state. `"done"` means a marker `D`
/// arrived (and `exit_is_known` says whether its code came with it);
/// `"settled"` is the silence fallback's honest "quiet, exit unknown".
fn block_status(block: &CommandBlock) -> &'static str {
    match block.status {
        BlockStatus::Done => "done",
        BlockStatus::Running => "running",
        BlockStatus::Settled => "settled",
        BlockStatus::Prompt => "prompt",
    }
}

/// Slice one command block into the `last_command` read's answer.
fn command_read(
    terminal_id: &str,
    seq: u64,
    track: &TerminalTrack,
    block: &CommandBlock,
) -> TerminalCommand {
    TerminalCommand {
        terminal_id: terminal_id.to_string(),
        status: block_status(block).to_string(),
        command: block.command.clone(),
        cwd: block.cwd.clone(),
        exit_code: if block.exit_is_known() {
            block.exit_code
        } else {
            None
        },
        marker_backed: block.marker_backed,
        seq,
        lines: track.mirror.block_lines(block),
    }
}
struct LiveTerminal {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    subscribers: Vec<mpsc::UnboundedSender<TerminalEvent>>,
    replay: VecDeque<TerminalEvent>,
    replay_bytes: usize,
    seq: u64,
    last_active_at: std::time::Instant,
    exited: bool,
    /// The grid size the PTY was last given (clamped).
    cols: u16,
    rows: u16,
    /// Opening cwd and shell basename — the identity `Terminals::list_terminals`
    /// reports.
    cwd: String,
    shell: String,
    /// The text mirror every read answers from.
    track: TerminalTrack,
}

impl LiveTerminal {
    /// Stamp a seq, append to the bounded replay window, and fan out to live
    /// subscribers. On `Exit` the subscriber senders are dropped so every
    /// attached stream ends after delivering the event.
    fn emit(&mut self, event: TerminalEvent) {
        self.last_active_at = std::time::Instant::now();
        let bytes = match &event {
            TerminalEvent::Data { data, .. } => data.len(),
            TerminalEvent::Exit { .. } => 16,
        };
        self.replay.push_back(event.clone());
        self.replay_bytes += bytes;
        while self.replay_bytes > MAX_REPLAY_BYTES && self.replay.len() > 1 {
            if let Some(dropped) = self.replay.pop_front() {
                self.replay_bytes -= match &dropped {
                    TerminalEvent::Data { data, .. } => data.len(),
                    TerminalEvent::Exit { .. } => 16,
                };
            }
        }
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
        if matches!(event, TerminalEvent::Exit { .. }) {
            self.exited = true;
            self.subscribers.clear();
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

struct TerminalsInner {
    sessions: Mutex<HashMap<String, Arc<Mutex<LiveTerminal>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct Terminals {
    inner: Arc<TerminalsInner>,
}

impl Default for Terminals {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.clamp(2, 500),
        rows: rows.clamp(1, 300),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The user's interactive shell: `$SHELL`, else the platform default.
fn selected_shell() -> String {
    if cfg!(windows) {
        return std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into());
    }
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".into()
            } else {
                "/bin/bash".into()
            }
        })
}

impl Terminals {
    /// Requires a tokio runtime (spawns the exited-session reaper).
    pub fn new() -> Self {
        let terminals = Self {
            inner: Arc::new(TerminalsInner {
                sessions: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(reaper_task(Arc::downgrade(&terminals.inner)));
        terminals
    }

    /// Open a login shell in `cwd`. The PTY outlives every subscriber; it dies on
    /// [`Self::close`], shell exit + TTL, or engine shutdown.
    pub fn open(&self, cwd: &str, cols: u16, rows: u16) -> Result<TerminalSession, EngineError> {
        self.open_impl(cwd, cols, rows, None)
    }

    /// Explicit shell override (tests use `/bin/sh`).
    pub fn open_with_shell(
        &self,
        cwd: &str,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
    ) -> Result<TerminalSession, EngineError> {
        self.open_impl(cwd, cols, rows, shell)
    }

    fn open_impl(
        &self,
        cwd: &str,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
    ) -> Result<TerminalSession, EngineError> {
        if lock(&self.inner.sessions).len() >= MAX_TERMINALS {
            return Err(EngineError::Other(format!(
                "Too many open terminals (maximum {MAX_TERMINALS})"
            )));
        }
        if !std::fs::metadata(cwd).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(EngineError::Other(
                "Session working directory is unavailable".into(),
            ));
        }

        let shell = shell.map(str::to_string).unwrap_or_else(selected_shell);
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| shell.clone());

        let pty = native_pty_system();
        let size = clamp_size(cols, rows);
        let pair = pty
            .openpty(size)
            .map_err(|e| EngineError::Other(format!("could not open a pty: {e}")))?;
        let mut cmd = CommandBuilder::new(&shell);
        // OSC 133 shell integration (step 5 A): the harness detects the REAL
        // shell and hands back launch changes that CHAIN the user's own
        // prompt machinery (`bash` → `--rcfile` wrapper that sources the
        // user's files first, `zsh` → ZDOTDIR stub dir, `fish` →
        // `--init-command`). Anything it does not know (`sh`, `dash`, …) and
        // the whole PowerShell/Windows case get NO injection — a declared
        // gap, and the parser's silence fallback covers those terminals.
        let integration = komet_harness::shell_env::integration::plan(&shell);
        let drop_login_flag = integration
            .as_ref()
            .is_some_and(|plan| plan.drop_login_flag);
        if !cfg!(windows) && !drop_login_flag {
            cmd.arg("-l"); // login shell — the user's real PATH/profile
        }
        if let Some(plan) = integration.as_ref() {
            for (key, value) in &plan.env {
                cmd.env(key, value);
            }
            for arg in &plan.extra_args {
                cmd.arg(arg);
            }
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Komet");
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| EngineError::Other(format!("could not spawn {shell_name}: {e}")))?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| EngineError::Other(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| EngineError::Other(format!("pty writer: {e}")))?;

        let id = new_id();
        let session = Arc::new(Mutex::new(LiveTerminal {
            master: pair.master,
            writer,
            killer,
            subscribers: Vec::new(),
            replay: VecDeque::new(),
            replay_bytes: 0,
            seq: 0,
            last_active_at: std::time::Instant::now(),
            exited: false,
            cols: size.cols,
            rows: size.rows,
            cwd: cwd.to_string(),
            shell: shell_name.clone(),
            track: TerminalTrack::new(size.cols, size.rows),
        }));
        lock(&self.inner.sessions).insert(id.clone(), session.clone());

        // Raw PTY bytes: blocking reader thread → batcher task (12ms windows).
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || read_pty(reader, raw_tx))
            .map_err(|e| EngineError::Other(format!("pty reader thread: {e}")))?;
        let wait = tokio::task::spawn_blocking(move || child.wait());
        tokio::spawn(pump_output(Arc::downgrade(&session), raw_rx, wait));

        Ok(TerminalSession {
            id,
            cwd: cwd.to_string(),
            shell: shell_name,
        })
    }

    fn session(&self, terminal_id: &str) -> Result<Arc<Mutex<LiveTerminal>>, EngineError> {
        lock(&self.inner.sessions)
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| EngineError::Other("Terminal not found".into()))
    }

    /// Replay (from `after_seq`, bounded 1MB window) then live tail. The stream
    /// ends after `Exit`; detaching (dropping the stream) leaves the PTY running.
    pub fn subscribe(
        &self,
        terminal_id: &str,
        after_seq: Option<u64>,
    ) -> Result<mpsc::UnboundedReceiver<TerminalEvent>, EngineError> {
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        session.last_active_at = std::time::Instant::now();
        let (tx, rx) = mpsc::unbounded_channel();
        let after = after_seq.unwrap_or(0);
        for event in &session.replay {
            let seq = match event {
                TerminalEvent::Data { seq, .. } | TerminalEvent::Exit { seq, .. } => *seq,
            };
            if seq > after {
                let _ = tx.send(event.clone());
            }
        }
        if !session.exited {
            session.subscribers.push(tx);
        }
        // On an exited session `tx` drops here: the stream ends after the replay.
        Ok(rx)
    }

    /// Write input bytes; `data` is base64 (matching `Data` events), with a plain
    /// UTF-8 fallback for lenient callers.
    pub fn write(&self, terminal_id: &str, data: &str) -> Result<(), EngineError> {
        let bytes = BASE64
            .decode(data)
            .unwrap_or_else(|_| data.as_bytes().to_vec());
        self.write_raw(terminal_id, &bytes)
    }

    /// The raw-PTY write path behind RPC `WriteTerminal` — the user's own
    /// keystrokes. No agent-reachable surface calls it.
    pub fn write_raw(&self, terminal_id: &str, bytes: &[u8]) -> Result<(), EngineError> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(EngineError::Other("Terminal input is too large".into()));
        }
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        if session.exited {
            return Err(EngineError::Other("Terminal has exited".into()));
        }
        session.last_active_at = std::time::Instant::now();
        session
            .writer
            .write_all(bytes)
            .and_then(|_| session.writer.flush())
            .map_err(|e| EngineError::Other(format!("Terminal write failed: {e}")))
    }

    pub fn resize(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<(), EngineError> {
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        session.last_active_at = std::time::Instant::now();
        if session.exited {
            return Ok(());
        }
        let size = clamp_size(cols, rows);
        session
            .master
            .resize(size)
            .map_err(|e| EngineError::Other(format!("Terminal resize failed: {e}")))?;
        session.cols = size.cols;
        session.rows = size.rows;
        // The mirror must follow the PTY size or every read after this would
        // describe the wrong wrapping. Stamped at the newest output seq: a
        // resize is not output, and mints no seq of its own.
        let seq = session.seq;
        session.track.resize(seq, size.cols, size.rows);
        Ok(())
    }

    /// Rows for `ListTerminals` and the agents' `terminal_list`: every terminal.
    pub fn list_terminals(&self) -> Vec<TerminalInfo> {
        let sessions: Vec<(String, Arc<Mutex<LiveTerminal>>)> = lock(&self.inner.sessions)
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        let mut infos = Vec::with_capacity(sessions.len());
        for (id, session) in sessions {
            let session = lock(&session);
            infos.push(TerminalInfo {
                terminal_id: id,
                title: session.track.mirror.title().map(str::to_string),
                cwd: session.cwd.clone(),
                shell: session.shell.clone(),
                cols: session.cols,
                rows: session.rows,
                seq: session.seq,
                tracked: true,
                exited: session.exited,
            });
        }
        infos
    }

    /// The `last_command` angle: the OSC 133 blocks only slice a terminal's own
    /// text, and a terminal without any markers refuses loudly instead of
    /// guessing where the prompt was.
    pub fn last_command(&self, terminal_id: &str) -> Result<TerminalCommand, EngineError> {
        let session = self.session(terminal_id)?;
        let session = lock(&session);
        let track = &session.track;
        if !track.mirror.saw_markers() {
            return Err(EngineError::Other(format!(
                "Terminal {terminal_id} has no command markers (OSC 133) — its shell did not load the Komet integration, so there is no honest boundary to slice `last_command` from; use the `tail` or `delta` angle"
            )));
        }
        let block = track.mirror.last_command_block().ok_or_else(|| {
            EngineError::Other(format!(
                "No command has run in terminal {terminal_id} since its markers began"
            ))
        })?;
        Ok(command_read(terminal_id, session.seq, track, &block))
    }

    /// Request 1 — the current screen in clear text, one entry per viewport row,
    /// top to bottom. This is the read for a full-screen program: its scrollback
    /// is a pile of escape drafts, its display is these rows. Errors when the
    /// terminal is not tracked (see [`Self::set_tracked`]).
    pub fn screen_text(&self, terminal_id: &str) -> Result<TerminalScreen, EngineError> {
        let session = self.session(terminal_id)?;
        let session = lock(&session);
        let track = &session.track;
        Ok(TerminalScreen {
            terminal_id: terminal_id.to_string(),
            cols: track.mirror.cols(),
            rows: track.mirror.rows(),
            seq: track.mirror.seq(),
            title: track.mirror.title().map(str::to_string),
            lines: track.mirror.screen_text(),
        })
    }

    /// Request 2 — the last `lines` lines of the terminal's text in clear text,
    /// oldest first (the scrollback tail, then the viewport). Asking for more
    /// than the mirror holds returns everything it has, so the count comes back in
    /// [`TerminalTail::requested`] rather than as padding.
    pub fn tail_text(&self, terminal_id: &str, lines: usize) -> Result<TerminalTail, EngineError> {
        let session = self.session(terminal_id)?;
        let session = lock(&session);
        let track = &session.track;
        Ok(TerminalTail {
            terminal_id: terminal_id.to_string(),
            seq: track.mirror.seq(),
            requested: lines,
            lines: track.mirror.tail_text(lines),
        })
    }

    /// Request 3 — the text that appeared after `since_seq`, which is the seq a
    /// previous [`Self::screen_text`] or [`Self::tail_text`] read reported. Steps
    /// newer than `since_seq` only; nothing follows an already-caught-up seq.
    ///
    /// `needs_snapshot` says a plain append cannot describe the range (the program
    /// rewrote the screen, the grid was resized, the alt screen flipped) and
    /// [`Self::screen_text`] is the right read; `truncated` says the journal no
    /// longer holds the whole range asked for.
    pub fn text_delta(
        &self,
        terminal_id: &str,
        since_seq: u64,
    ) -> Result<TerminalDelta, EngineError> {
        let session = self.session(terminal_id)?;
        let session = lock(&session);
        let track = &session.track;
        Ok(terminal_delta(
            terminal_id,
            track.mirror.delta_since(since_seq),
        ))
    }

    /// Kill the shell (if still running) and drop the session + replay buffer.
    pub fn close(&self, terminal_id: &str) -> Result<(), EngineError> {
        let session = lock(&self.inner.sessions)
            .remove(terminal_id)
            .ok_or_else(|| EngineError::Other("Terminal not found".into()))?;
        dispose(&session, true);
        Ok(())
    }

    /// Any live PTY (the reaper prunes exited ones) — restarts kill shells, so
    /// the auto-updater waits for none.
    pub fn any_open(&self) -> bool {
        !lock(&self.inner.sessions).is_empty()
    }

    /// Engine shutdown: kill every live shell.
    pub fn shutdown(&self) {
        let sessions: Vec<_> = lock(&self.inner.sessions).drain().map(|(_, s)| s).collect();
        for session in sessions {
            dispose(&session, true);
        }
    }
}

fn dispose(session: &Arc<Mutex<LiveTerminal>>, kill: bool) {
    let mut session = lock(session);
    session.subscribers.clear();
    if kill
        && !session.exited
        && let Err(err) = session.killer.kill()
    {
        tracing::debug!(error = %err, "terminal kill failed (already exited?)");
    }
}

/// Blocking PTY reader: forwards raw chunks until EOF. A closed PTY reads as an
/// error on some platforms (EIO on Linux once the shell exits) — both end the loop.
fn read_pty(mut reader: Box<dyn Read + Send>, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Batches raw chunks into `Data` events every [`TERMINAL_OUTPUT_BATCH_MS`], then —
/// once the reader hits EOF (shell gone) — emits the final `Exit` event. Holds only
/// a weak session handle so a closed terminal tears this task down.
async fn pump_output(
    session: Weak<Mutex<LiveTerminal>>,
    mut raw_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    wait: tokio::task::JoinHandle<Result<portable_pty::ExitStatus, std::io::Error>>,
) {
    let batch = Duration::from_millis(TERMINAL_OUTPUT_BATCH_MS);
    let emit = |buffer: Vec<u8>| -> bool {
        let Some(session) = session.upgrade() else {
            return false;
        };
        let mut session = lock(&session);
        let seq = session.next_seq();
        // The mirror folds the exact bytes of this batch, under the same lock
        // and with the same seq the subscribers are handed — so a text read
        // and a replay line up.
        session.track.feed(seq, &buffer);
        session.emit(TerminalEvent::Data {
            seq,
            data: BASE64.encode(&buffer),
        });
        true
    };
    'outer: while let Some(first) = raw_rx.recv().await {
        let mut buffer = first;
        let deadline = tokio::time::Instant::now() + batch;
        loop {
            match tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
                Ok(None) => {
                    // Reader gone: flush, then fall through to the exit stamp.
                    emit(buffer);
                    break 'outer;
                }
                Err(_) => break, // batch window elapsed
            }
        }
        if !emit(buffer) {
            return; // terminal closed underneath us
        }
    }
    let exit_code = match wait.await {
        Ok(Ok(status)) => status.exit_code() as i32,
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "terminal wait failed");
            -1
        }
        Err(err) => {
            tracing::debug!(error = %err, "terminal wait task failed");
            -1
        }
    };
    if let Some(session) = session.upgrade() {
        let mut session = lock(&session);
        let seq = session.next_seq();
        session.emit(TerminalEvent::Exit {
            seq,
            exit_code,
            signal: None,
        });
    }
}

/// Live shells never expire on idleness — a detached session is the user's running
/// process. Only EXITED sessions are swept after [`EXITED_TTL`]: they're inert
/// replay buffers held so a returning viewer can show the tail + exit status.
async fn reaper_task(inner: Weak<TerminalsInner>) {
    let mut tick = tokio::time::interval(REAPER_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { break };
        let mut sessions = lock(&inner.sessions);
        sessions.retain(|_, session| {
            let session = lock(session);
            !(session.exited && session.last_active_at.elapsed() > EXITED_TTL)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a tracked mirror the way the output pump does: one seq per batch.
    fn scripted(cols: u16, rows: u16) -> TerminalTrack {
        TerminalTrack::new(cols, rows)
    }

    fn text(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    // ---- the three reads, on a scripted byte stream (no PTY, no UI) ----

    #[test]
    fn request_1_and_2_answer_in_clear_text() {
        let mut track = scripted(40, 4);
        track.feed(1, b"$ ");
        track.feed(2, b"ls\r\nnotes.txt\r\n");

        let screen = track.mirror.screen_text();
        assert_eq!(screen[0], "$ ls");
        assert_eq!(screen[1], "notes.txt");
        // Clear text: no escape bytes survive into a read.
        assert!(
            screen
                .iter()
                .all(|line| !line.contains('\x1b') && !line.contains('\r')),
            "{screen:?}"
        );

        // Request 2 hands back the tail, oldest first, viewport last. The cursor
        // sits on the row after `notes.txt` (the batch ended with CRLF), so the
        // tail ends on the empty rows the grid still holds.
        assert_eq!(track.mirror.tail_text(2), text(&["", ""]));
        assert_eq!(track.mirror.tail_text(3), text(&["notes.txt", "", ""]));
        let everything = track.mirror.tail_text(40);
        assert_eq!(everything.first().unwrap(), "$ ls");
        assert_eq!(track.mirror.seq(), 2);
    }

    #[test]
    fn request_3_reports_only_what_followed_the_seq() {
        let mut track = scripted(40, 4);
        for (ix, line) in ["first", "second", "third"].iter().enumerate() {
            track.feed(ix as u64 + 1, format!("{line}\r\n").as_bytes());
        }

        let delta = track.mirror.delta_since(2);
        assert_eq!(delta.from_seq, 2);
        assert_eq!(delta.through_seq, 3);
        assert_eq!(delta.lines, text(&["third"]), "not the lines before seq 2");
        assert!(!delta.needs_snapshot);
        assert!(!delta.truncated);

        // Already caught up: nothing follows.
        assert!(track.mirror.delta_since(3).lines.is_empty());
        // From before the first byte: the whole transcript, in order.
        assert_eq!(
            track.mirror.delta_since(0).lines,
            text(&["first", "second", "third"])
        );
    }

    /// A full-screen program repaints the same rows, so the screen read is the
    /// one that stays true; the journal flags the repaint instead of pretending
    /// the repaint was an append.
    #[test]
    fn request_1_is_the_read_for_a_full_screen_app() {
        let mut track = scripted(20, 3);
        track.feed(1, b"leftover output\r\n");
        track.feed(2, b"\x1b[?1049h\x1b[Hstatus: running\r\ncol a   col b");
        assert_eq!(
            track.mirror.screen_text(),
            text(&["status: running", "col a   col b", ""])
        );
        assert!(
            track.mirror.delta_since(1).needs_snapshot,
            "entering the alt screen is not an append"
        );
        // The app repaints its status row, erasing the rest of the line the way
        // a TUI does (without `[K` the longer old text would still show).
        track.feed(3, b"\x1b[Hstatus: done\x1b[K");
        assert_eq!(track.mirror.screen_text()[0], "status: done");
        assert!(track.mirror.delta_since(2).needs_snapshot);
        // Leaving restores the shell screen.
        track.feed(4, b"\x1b[?1049l");
        assert_eq!(track.mirror.screen_text()[0], "leftover output");
    }

    /// A resize is not output: it is stamped at the newest output seq and must
    /// still invalidate a caller that is caught up on that seq.
    #[test]
    fn a_resize_is_folded_at_the_newest_output_seq() {
        let mut track = scripted(20, 3);
        track.feed(1, b"header\r\nbody");
        assert_eq!(track.mirror.screen_text(), text(&["header", "body", ""]));

        track.resize(1, 40, 3);
        assert_eq!(track.mirror.cols(), 40);
        let delta = track.mirror.delta_since(1);
        assert!(delta.lines.is_empty(), "no output followed seq 1");
        assert!(
            delta.needs_snapshot,
            "but the wrapping changed, so the caller's text is stale"
        );

        // Output after the resize diffs cleanly again.
        track.feed(2, b"\r\nfooter");
        let after = track.mirror.delta_since(1);
        assert_eq!(after.lines, text(&["footer"]));
    }

    // ---- the wiring: the pump feeds the mirror, and only when asked to ----

    async fn open_sh(terminals: &Terminals, cwd: &str) -> TerminalSession {
        terminals
            .open_with_shell(cwd, 80, 24, Some("/bin/sh"))
            .expect("open sh")
    }

    fn decoded(events: &[TerminalEvent]) -> String {
        let mut out = Vec::new();
        for event in events {
            if let TerminalEvent::Data { data, .. } = event {
                out.extend(BASE64.decode(data).expect("valid base64"));
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// Drain until the transcript holds `marker`. Because the pump feeds the
    /// mirror under the same lock, before the event is sent, seeing the event
    /// means the mirror already has those bytes.
    async fn drain_until(
        rx: &mut mpsc::UnboundedReceiver<TerminalEvent>,
        events: &mut Vec<TerminalEvent>,
        marker: &str,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !decoded(events).contains(marker) {
            let event = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("terminal event before timeout")
                .expect("terminal stream alive");
            events.push(event);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_untracked_terminal_is_not_emulated_and_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let terminals = Terminals::new();
        let session = open_sh(&terminals, &tmp.path().to_string_lossy()).await;
        // Normalize against a developer running the suite with the env switch on.
        terminals.set_tracked(&session.id, false).unwrap();

        assert!(!terminals.is_tracked(&session.id).expect("is_tracked"));
        // Refusing loudly beats answering with an empty screen.
        for result in [
            terminals.screen_text(&session.id).map(|_| ()),
            terminals.tail_text(&session.id, 10).map(|_| ()),
            terminals.text_delta(&session.id, 0).map(|_| ()),
        ] {
            let err = result.expect_err("reads on an untracked terminal must fail");
            assert!(
                err.to_string().contains("not tracked"),
                "explicit error, got: {err}"
            );
        }

        // The terminal itself is unaffected: output still streams.
        let mut rx = terminals.subscribe(&session.id, None).expect("subscribe");
        let mut events = Vec::new();
        terminals
            .write(&session.id, &BASE64.encode("echo un1racked\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "un1racked").await;
        terminals.close(&session.id).expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tracking_follows_live_output_and_the_delta_stays_in_range() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let terminals = Terminals::new();
        let session = open_sh(&terminals, &tmp.path().to_string_lossy()).await;
        let mut rx = terminals.subscribe(&session.id, None).expect("subscribe");
        let mut events = Vec::new();

        terminals.set_tracked(&session.id, true).expect("track");
        assert!(terminals.is_tracked(&session.id).unwrap());

        terminals
            .write(&session.id, &BASE64.encode("echo on7 tracked\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "on7 tracked").await;

        let screen = terminals.screen_text(&session.id).expect("screen");
        assert_eq!(screen.rows, 24);
        assert!(
            screen.lines.iter().any(|line| line.contains("on7 tracked")),
            "the live output should be on the mirrored screen: {:?}",
            screen.lines
        );
        let seq_after_first = screen.seq;
        let tail = terminals
            .tail_text(&session.id, 24)
            .expect("tail on a tracked terminal");
        assert_eq!(tail.requested, 24);
        assert_eq!(tail.lines.len(), 24, "a fresh grid holds exactly its rows");

        // A second command: the delta since the first read must carry the second
        // and not repeat the first.
        terminals
            .write(&session.id, &BASE64.encode("echo lat9r than seq\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "lat9r than seq").await;
        let delta = terminals
            .text_delta(&session.id, seq_after_first)
            .expect("delta");
        assert_eq!(delta.from_seq, seq_after_first);
        assert!(delta.through_seq > seq_after_first);
        assert!(
            delta.lines.iter().any(|line| line.contains("lat9r")),
            "the delta must carry what followed: {:?}",
            delta.lines
        );
        assert!(
            !delta.lines.iter().any(|line| line.contains("on7")),
            "and nothing that came before seq {}: {:?}",
            seq_after_first,
            delta.lines
        );

        // Turning tracking off frees the mirror and the reads refuse again.
        terminals.set_tracked(&session.id, false).expect("untrack");
        assert!(terminals.screen_text(&session.id).is_err());
        terminals.close(&session.id).expect("close");
    }

    /// Enabling tracking mid-stream replays the bytes still in the window, so the
    /// first read is not an empty screen that only fills in from the next key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabling_tracking_replays_the_retained_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let terminals = Terminals::new();
        let session = open_sh(&terminals, &tmp.path().to_string_lossy()).await;
        let mut rx = terminals.subscribe(&session.id, None).expect("subscribe");
        let mut events = Vec::new();

        // Output lands while nothing is mirrored.
        terminals
            .write(&session.id, &BASE64.encode("echo earli3r replayed\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "earli3r replayed").await;

        terminals.set_tracked(&session.id, true).expect("track");
        let screen = terminals.screen_text(&session.id).expect("screen");
        assert!(
            screen
                .lines
                .iter()
                .any(|line| line.contains("earli3r replayed")),
            "the replay should have been folded in: {:?}",
            screen.lines
        );
        // The seq the caller is holding from the replay is the live one.
        assert!(
            terminals
                .text_delta(&session.id, screen.seq)
                .expect("delta")
                .lines
                .is_empty(),
            "nothing has followed the replayed window"
        );
        terminals.close(&session.id).expect("close");
    }

    // ---- the agent gate: a human decision, never an agent's own ----

    /// The core guard: a terminal whose mirror is fully fed is STILL refused to
    /// agents until a human grants the read. Tracking is not visibility.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_reads_refuse_a_terminal_the_human_never_marked_visible() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let terminals = Terminals::new();
        let session = open_sh(&terminals, &tmp.path().to_string_lossy()).await;
        let mut rx = terminals.subscribe(&session.id, None).expect("subscribe");
        let mut events = Vec::new();

        // Feed the mirror the way a curious agent would find it: tracked, with
        // live output, but nobody ever granted visibility.
        terminals.set_tracked(&session.id, true).expect("track");
        terminals
            .write(&session.id, &BASE64.encode("echo g0 tesla-gate\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "g0 tesla-gate").await;
        assert!(
            terminals
                .screen_text(&session.id)
                .expect("ungated read works")
                .lines
                .iter()
                .any(|line| line.contains("g0 tesla-gate")),
            "the mirror IS fed — the refusal below is the gate, not emptiness"
        );

        assert!(
            !terminals
                .is_agent_visible(&session.id)
                .expect("default deny")
        );
        for result in [
            terminals.agent_screen_text(&session.id).map(|_| ()),
            terminals.agent_tail_text(&session.id, 10).map(|_| ()),
            terminals.agent_text_delta(&session.id, 0).map(|_| ()),
        ] {
            let err = result.expect_err("gated reads on an invisible terminal must fail");
            assert!(
                err.to_string().contains("not visible to agents"),
                "explicit denial, got: {err}"
            );
        }
        terminals.close(&session.id).expect("close");
    }

    /// The grant is also the switch that makes the feature real: it turns
    /// tracking on (so the first read answers, and never sits in the
    /// grant-with-no-mirror no-op state), and revoking drops the mirror so no
    /// clear-text scrollback lingers for a terminal agents cannot read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn granting_visibility_switches_tracking_on_and_reads_answer() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let terminals = Terminals::new();
        let session = open_sh(&terminals, &tmp.path().to_string_lossy()).await;
        // Normalize against a developer running the suite with the env switch on.
        terminals.set_tracked(&session.id, false).expect("untrack");
        let mut rx = terminals.subscribe(&session.id, None).expect("subscribe");
        let mut events = Vec::new();

        terminals
            .set_agent_visible(&session.id, true)
            .expect("grant");
        assert!(
            terminals.is_tracked(&session.id).expect("tracked"),
            "the grant must switch the mirror on or every read would fail"
        );

        terminals
            .write(&session.id, &BASE64.encode("echo vis1ble grant\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "vis1ble grant").await;
        let screen = terminals
            .agent_screen_text(&session.id)
            .expect("gated screen read");
        assert!(
            screen
                .lines
                .iter()
                .any(|line| line.contains("vis1ble grant")),
            "{:?}",
            screen.lines
        );
        assert!(
            !terminals
                .agent_tail_text(&session.id, 5)
                .expect("gated tail read")
                .lines
                .is_empty()
        );
        let seq = screen.seq;
        terminals
            .write(&session.id, &BASE64.encode("echo lat9r than grant\n"))
            .expect("write");
        drain_until(&mut rx, &mut events, "lat9r than grant").await;
        let delta = terminals
            .agent_text_delta(&session.id, seq)
            .expect("gated delta read");
        assert!(
            delta
                .lines
                .iter()
                .any(|line| line.contains("lat9r than grant")),
            "{:?}",
            delta.lines
        );

        // Revoking the grant: mirror gone AND the gate closed again.
        terminals
            .set_agent_visible(&session.id, false)
            .expect("revoke");
        assert!(!terminals.is_tracked(&session.id).expect("untracked"));
        let err = terminals
            .agent_screen_text(&session.id)
            .expect_err("revoked terminal is refused");
        assert!(
            err.to_string().contains("not visible to agents"),
            "the gate answers before the tracking check: {err}"
        );
        terminals.close(&session.id).expect("close");
    }

    /// `ListTerminals` state: identity, size and both switches, without a
    /// track even being needed — the UI/step-6 toggle reads this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_terminals_reports_identity_visibility_and_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let terminals = Terminals::new();
        let session = open_sh(&terminals, &tmp.path().to_string_lossy()).await;
        terminals.set_tracked(&session.id, false).expect("untrack");

        let info = terminals
            .list_terminals()
            .into_iter()
            .find(|info| info.terminal_id == session.id)
            .expect("listed");
        assert_eq!(info.cwd, tmp.path().to_string_lossy());
        assert_eq!(info.shell, "sh");
        assert_eq!((info.cols, info.rows), (80, 24));
        assert!(!info.tracked);
        assert!(!info.agent_visible, "agents see nothing by default");
        assert!(!info.exited);

        terminals
            .set_agent_visible(&session.id, true)
            .expect("grant");
        let info = terminals
            .list_terminals()
            .into_iter()
            .find(|info| info.terminal_id == session.id)
            .expect("still listed");
        assert!(info.agent_visible);
        assert!(info.tracked, "the grant switched the mirror on");

        terminals.close(&session.id).expect("close");
        assert!(
            !terminals
                .list_terminals()
                .iter()
                .any(|info| info.terminal_id == session.id)
        );
    }

    /// The command-capture claim the harness pipe tests CANNOT make: with
    /// stdin a pipe, bash echoes nothing between the prompt-end `B` and the
    /// output-start `C` (no readline echo), so the typed text never reaches
    /// the grid. This drives bash over a REAL PTY exactly the way
    /// [`Terminals::open_impl`] launches it — plan's `--rcfile` wrapper, no
    /// `-l` (the wrapper emulates login), `TERM=xterm-256color` — and types
    /// `echo komet-visible` through the PTY writer, so the kernel echoes it
    /// the way a human's keystrokes are echoed in production. The question
    /// this answers: does an agent reading `last_command` on a real Komet
    /// terminal see the command, or see the prompt?
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_injection_on_a_real_pty_reports_the_typed_command_to_agents() {
        if !std::path::Path::new("/bin/bash").is_file() {
            eprintln!("no bash on this machine");
            return;
        }
        let home = tempfile::tempdir().expect("fake HOME");
        std::fs::write(home.path().join(".bashrc"), "PS1='> '\n").expect("write bashrc");
        let plan = komet_harness::shell_env::integration::plan("/bin/bash").expect("bash plan");
        // Same launch shape as open_impl: the wrapper emulates the login, so
        // no `-l` is passed.
        assert!(plan.drop_login_flag && plan.extra_args.len() == 2);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("/bin/bash");
        for (key, value) in &plan.env {
            cmd.env(key, value);
        }
        for arg in &plan.extra_args {
            cmd.arg(arg);
        }
        cmd.env("HOME", home.path());
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(home.path());
        let mut child = pair.slave.spawn_command(cmd).expect("spawn bash on a pty");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let mut writer = pair.master.take_writer().expect("pty writer");

        // Blocking pty read into a shared buffer, the way `read_pty` does.
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let bytes = Arc::clone(&bytes);
            std::thread::Builder::new()
                .name("pty-probe-read".into())
                .spawn(move || {
                    let mut chunk = [0u8; 8192];
                    while let Ok(n) = reader.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        bytes
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .extend_from_slice(&chunk[..n]);
                    }
                })
                .expect("reader thread");
        }
        let snapshot = || bytes.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let contains =
            |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
        let wait_until = |pred: &dyn Fn(&[u8]) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                let now = snapshot();
                if pred(&now) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "pty output stalled");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        // The first prompt must fully paint (A … B) before typing.
        wait_until(&|b| contains(b, b"\x1b]133;B"));
        writer
            .write_all(b"echo komet-visible\n")
            .expect("type into the pty");
        // `D;0` AFTER the echoed line: the command finished and its exit code
        // rode the marker into the stream. (A `D;0` already exists at the
        // first prompt — bash reports the empty command before the injection
        // loaded — so the search starts at `komet-visible`, which only the
        // echo or the output can print.)
        wait_until(&|b| {
            b.windows(b"komet-visible".len())
                .position(|w| w == b"komet-visible")
                .is_some_and(|at| contains(&b[at + b"komet-visible".len()..], b"\x1b]133;D;0"))
        });
        let _ = child.kill();

        let mut emu = Emulator::with_scrollback(80, 24, 1000);
        emu.feed(&snapshot());
        let blocks = emu.command_blocks();
        let typed = blocks
            .iter()
            .find(|block| {
                block.command.as_deref() == Some("echo komet-visible") && block.exit_code == Some(0)
            })
            .unwrap_or_else(|| panic!("a real PTY must carry the typed line: {blocks:?}"));
        // `B` marked the prompt end, so the command is the TYPED text of that
        // line — not the prompt line itself (`LineWithPrompt` is the pipe-mode
        // artifact the harness test can no longer claim to prove).
        assert_eq!(typed.command_source, komet_term::CommandSource::Typed);
        assert!(typed.marker_backed);
        assert_eq!(
            emu.block_lines(typed),
            vec!["komet-visible".to_string()],
            "the block's output is the echo, not the prompt"
        );
    }
}
