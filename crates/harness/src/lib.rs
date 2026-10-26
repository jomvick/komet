//! komet-harness — one interface over coding agents (plus a mock for tests).
//!
//! NATIVE DRIVERS speak each agent's own wire directly: Claude Code over
//! stream-json ([`ClaudeHarness`]), Codex over the app-server JSON-RPC
//! ([`CodexHarness`]), Cursor through a pinned @cursor/sdk shim
//! ([`CursorHarness`]), and Muse Code over its Muse Session Protocol
//! ([`MuseHarness`]). The shared [`AcpHarness`] remains ONLY for agents
//! built ground-up on ACP — Devin (`devin acp`), GitHub Copilot
//! (`copilot --acp`), Grok (`grok agent stdio`), Hermes (`hermes acp`) and
//! opencode (`opencode acp`) — plus pi via the community `pi-acp` adapter
//! until a native driver exists. Adapter-mediated ACP for
//! claude/codex/cursor was retired: the
//! adapters held prompt turns open for background work the
//! CLIs themselves settle eagerly, manufacturing done-status bugs the
//! native wires don't have (decision record: docs/research/acp.md).

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
pub use tokio_util::sync::CancellationToken;

use komet_proto::{
    AgentEvent, ContextUsage, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, UserInputAnswer, UserInputQuestion,
};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness binary not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    /// A managed adapter install (npm) failed; carries npm's own output so
    /// the cause is diagnosable from the chat error alone.
    #[error("adapter install failed: {0}")]
    Install(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A steer prompt pushed into a live run; delivered at the harness's steering boundary.
pub struct SteerMessage {
    pub prompt: String,
    pub message_id: Option<String>,
}

pub type RequestInputFn =
    Box<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync>;

pub type RequestPermissionFn = Box<
    dyn Fn(
            komet_proto::PermissionKind,
            String,
            Vec<komet_proto::PermissionChoice>,
        ) -> oneshot::Receiver<komet_proto::PermissionChoice>
        + Send
        + Sync,
>;

/// Host-side controls handed to a run: input-request bridge + steering mailbox.
pub struct RunControls {
    /// The run sends questions and awaits answers (blocks the agent, mirrors komet).
    pub request_input: RequestInputFn,
    /// Permission bridge (Paseo `respondToPermission`): the run asks "may I
    /// run this tool/command", the host surfaces `PermissionRequested`, parks
    /// the resolver, and the user's decision comes back as the choice. A
    /// dropped receiver (run died) degrades to Deny on the harness side.
    pub request_permission: RequestPermissionFn,
    /// Steer prompts consumed at step/turn boundaries.
    pub steering: mpsc::Receiver<SteerMessage>,
    /// Cancel to interrupt the live run: the harness sends its protocol-level
    /// interrupt, then escalates to SIGTERM/SIGKILL on the child after a grace
    /// period. The run's stream ends with `Done { status: Interrupted }`.
    pub interrupt: CancellationToken,
}

impl RunControls {
    pub fn noop_permission() -> Box<
        dyn Fn(
                komet_proto::PermissionKind,
                String,
                Vec<komet_proto::PermissionChoice>,
            ) -> oneshot::Receiver<komet_proto::PermissionChoice>
            + Send
            + Sync,
    > {
        Box::new(|_kind, _summary, _choices| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(komet_proto::PermissionChoice::Deny);
            rx
        })
    }
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    fn display_name(&self) -> &str;
    fn supports_steering(&self) -> bool;
    fn steering_mode(&self) -> SteeringMode;
    /// Reasoning-effort ladder this harness supports, ordered weakest to
    /// strongest. UI ids map through
    /// [`ReasoningLevel::from_thinking_id`]/[`ReasoningLevel::as_thinking_id`]
    /// (komet-proto).
    fn reasoning_levels(&self) -> &[ReasoningLevel];
    /// Whether the agent's own CLI is present on this device — the settings
    /// gate for enabling the harness. A filesystem probe, never a spawn.
    /// Defaults to true for harnesses without a CLI to check (mock).
    fn installed(&self) -> bool {
        true
    }
    /// Whether every turn shape — user-prompted AND agent-initiated
    /// (background-subagent wakes) — ends with a deterministic `Done` from
    /// the agent's own wire. Native drivers reading the CLI's terminal frame
    /// directly return true, and the engine retires its quiesce watchdogs
    /// for them; adapter-mediated ACP agents keep the watchdog backstop.
    fn deterministic_turn_end(&self) -> bool {
        false
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError>;
    /// Bypass any in-process discovery memo and query the CLI again. The
    /// engine uses this at app startup and on explicit refreshes; adapters
    /// that do not memoize may use the default.
    async fn refresh_models(&self) -> Result<Vec<Model>, HarnessError> {
        self.models().await
    }
    /// Last-resort DISPLAY list used when live discovery fails and nothing has
    /// ever been cached: the harness's curated catalog, for the CLIs that
    /// still ship one. Display only — the engine keeps re-probing in the
    /// background and swaps this out the moment the CLI answers, and the send
    /// path still refuses any id the CLI does not advertise, so a fallback row
    /// can never smuggle a dead model into a run. Empty for adapter agents
    /// with no catalog (ACP: grok, hermes, opencode, cline).
    fn fallback_models(&self) -> Vec<Model> {
        Vec::new()
    }
    /// Slash commands the agent advertises for `cwd` (the chat's project, or
    /// HOME when none is selected). Native harnesses read the running CLI's
    /// live surface (ACP `availableCommands`, Claude `initialize` / skills,
    /// Codex `skills/list`). Empty only for harnesses without a command
    /// surface.
    async fn commands(
        &self,
        _cwd: Option<&std::path::Path>,
    ) -> Result<Vec<SlashCommand>, HarnessError> {
        Ok(Vec::new())
    }
    /// Out-of-session probe (Claude skill discovery via Bucket C
    /// `get_context_usage`). The live footer ring reads
    /// [`AgentEvent::ContextWindow`] from the running session, not this.
    async fn context_usage(&self) -> Result<ContextUsage, HarnessError> {
        Ok(ContextUsage::default())
    }
    /// Run one (persistent) session; the stream ends with `AgentEvent::Done`.
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

/// The cwd a command probe should run in — the chat's project, else HOME.
pub(crate) fn discovery_cwd(cwd: Option<&std::path::Path>) -> std::path::PathBuf {
    cwd.map(std::path::Path::to_path_buf).unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/"))
    })
}

/// Cache buster for a harness's command surface: the newest mtime over the
/// command/skill dirs that harness reads, under `cwd` and `$HOME`. A CLI-side
/// add/remove bumps a directory's mtime and invalidates the cache; 0 when no
/// candidate dir exists.
pub fn commands_buster(cwd: &std::path::Path, harness: HarnessId) -> u128 {
    let dirs: &[&str] = match harness {
        HarnessId::Opencode => &[".opencode/command", ".opencode/skills"],
        HarnessId::ClaudeCode => &[".claude/commands", ".claude/skills"],
        HarnessId::Codex => &[".codex"],
        _ => &[],
    };
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut newest: u128 = 0;
    for base in std::iter::once(cwd.to_path_buf()).chain(home) {
        for dir in dirs {
            let Ok(modified) = std::fs::metadata(base.join(dir)).and_then(|m| m.modified()) else {
                continue;
            };
            let stamp = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            newest = newest.max(stamp);
        }
    }
    newest
}

pub mod acp;
pub(crate) mod adapter_install;
pub mod antigravity;
pub mod capabilities;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod errors;
pub(crate) mod jsonrpc;
pub mod mock;
mod muse;
pub mod shell_env;

/// Bin directories where npm-installed CLIs land under Node version managers.
/// GUI launches never see these on PATH — the managers shape PATH in shell
/// init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
pub(crate) fn node_version_manager_bins() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    // fnm: `aliases/default` is a stable symlink to the active default
    // installation (the multishell PATH entries are ephemeral, per-shell).
    let mut fnm_roots: Vec<PathBuf> = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = &home {
        fnm_roots.push(home.join(".local").join("share").join("fnm"));
        fnm_roots.push(home.join("Library").join("Application Support").join("fnm"));
        fnm_roots.push(home.join(".fnm"));
    }
    for root in fnm_roots {
        dirs.push(root.join("aliases").join("default").join("bin"));
    }
    if let Some(home) = &home {
        // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join("Library").join("pnpm"));
        dirs.push(home.join(".local").join("share").join("pnpm"));
        // nvm: every installed version's bin, newest first.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> =
                entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            dirs.append(&mut versions);
        }
    }
    dirs
}

/// Resolve an agent CLI through the app PATH, login-shell PATH, and common
/// per-user install locations used by the harness drivers.
pub fn find_cli_path(binary: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>());
    let mut search = dirs.by_ref().collect::<Vec<_>>();
    if let Some(path) = shell_env::login_shell_path() {
        search.extend(std::env::split_paths(path));
    }
    search.extend(node_version_manager_bins());
    if let Some(home) = &home {
        search.extend([
            home.join(".local/bin"),
            home.join(".npm-global/bin"),
            home.join(".opencode/bin"),
            home.join(".grok/bin"),
            home.join(".hermes/bin"),
            home.join(".cursor/bin"),
        ]);
    }
    search.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);

    #[cfg(windows)]
    let mut candidates = search.into_iter().flat_map(|dir| {
        let exe = dir.join(binary);
        [
            exe.clone(),
            exe.with_extension("exe"),
            exe.with_extension("cmd"),
        ]
    });
    #[cfg(not(windows))]
    let mut candidates = search.into_iter().map(|dir| dir.join(binary));

    candidates.find(|candidate| candidate.is_file())
}

/// Add the login shell's PATH to a child process while preserving the PATH of
/// the current process. This lets GUI/service launches find user-installed
/// CLIs such as Homebrew's `gh` without changing the daemon's own environment.
pub fn compose_login_shell_path(cmd: &mut tokio::process::Command) {
    compose_path(cmd, std::iter::empty());
}

/// Compose the child's PATH: the resolved executable's directory first, then
/// our own PATH, then the login-shell PATH snapshot — deduped. npm-shim CLIs
/// are `#!/usr/bin/env node` scripts whose `node` lives beside them in the
/// version manager's bin dir, and the CLIs themselves shell out to tools
/// (git, rg, node) that a GUI/service launch's own PATH may lack.
pub(crate) fn compose_child_path(cmd: &mut tokio::process::Command, exe: &std::path::Path) {
    compose_path(cmd, exe.parent().filter(|d| !d.as_os_str().is_empty()));
}

fn compose_path<'a>(
    cmd: &mut tokio::process::Command,
    executable_dir: impl IntoIterator<Item = &'a std::path::Path>,
) {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for dir in executable_dir {
        paths.push(dir.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(shell_path) = shell_env::login_shell_path() {
        paths.extend(std::env::split_paths(shell_path));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// the proper background-crash message old komet showed (user requirement).
#[derive(Clone, Default)]
pub(crate) struct StderrTail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl StderrTail {
    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub(crate) fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        joined.truncate(Self::KEEP_BYTES * 2);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
pub(crate) fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// The full crash message: status plus the stderr tail when there is one.
pub(crate) fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!("{name} exited unexpectedly ({status}): {tail}"),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}

pub use acp::AcpHarness;
pub use antigravity::AntigravityHarness;
pub use claude::ClaudeHarness;
pub use codex::CodexHarness;
pub use cursor::CursorHarness;
pub use muse::MuseHarness;

// ---------------------------------------------------------------------------
// Child lifecycle (shared by the codex and ACP harnesses)
// ---------------------------------------------------------------------------

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
pub(crate) async fn shutdown_child(
    child: &mut tokio::process::Child,
    kill_grace: std::time::Duration,
) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
pub(crate) enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
pub(crate) fn send_signal(_pid: u32, _signal: Signal) {
    // No SIGTERM off unix; `start_kill`/`kill_on_drop` handle termination.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_buster_tracks_directory_changes() {
        let dir = tempfile::tempdir().unwrap();
        let commands = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&commands).unwrap();

        let before = commands_buster(dir.path(), HarnessId::ClaudeCode);
        assert!(before > 0, "an existing command dir must produce a buster");

        // Adding a command file bumps the directory mtime; set it explicitly
        // so a coarse filesystem clock cannot hide the change.
        std::fs::write(commands.join("review.md"), "# review").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        touch(&commands, later);

        let after = commands_buster(dir.path(), HarnessId::ClaudeCode);
        assert!(after > before, "buster must change: {before} -> {after}");
    }

    /// `std::fs::File::set_times` keeps the test dependency-free; opening
    /// read-only also works for a directory on the platforms komet builds for.
    fn touch(path: &std::path::Path, time: std::time::SystemTime) {
        std::fs::File::open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(time))
            .unwrap();
    }
}
