//! Cursor harness: drives Cursor's agent runtime through the PINNED
//! `@cursor/sdk` via a thin komet-owned Node shim (`shim.mjs`, JSONL over
//! stdio) — NOT over ACP, and NOT over `cursor-agent`'s print surface.
//!
//! Why: Cursor's ACP surface is lossy (subagent transcripts are stripped at
//! the boundary — verified live) and Cursor points integrators at the SDK.
//! The SDK does not wrap the cursor-agent binary at all: it is the agent
//! runtime bundled in-process, speaking proprietary protobuf/ConnectRPC to
//! Cursor's backend with client-side tool execution — there is no speakable
//! stdio wire to drive from Rust, so the shim IS the wire. The print surface
//! (`cursor-agent -p --trust`) is permission-free and is never used.
//!
//! VERSION PIN: [`CURSOR_SDK_PIN`]. The SDK is PUBLIC BETA with ~weekly
//! releases; the shim maps only the update kinds this pin ships and ignores
//! unknown ones, so churn degrades output rather than erroring the chat.
//! Revalidate the shim against the typings on every bump.
//!
//! - The shim is materialized into the SDK's managed npm install
//!   (`~/.komet/adapters/…`, [`crate::adapter_install::ensure_installed_shim`])
//!   and spawned as `node <shim>`.
//! - Done = the SDK run's terminal result (`turn` frame off `run.wait()` /
//!   `turn-ended`) — a crisp turn end by construction.
//! - Subagents: the SDK streams the FULL nested transcript as
//!   `tool-call-delta { callId, taskUpdate }`; the shim tags those frames
//!   with the spawning task's call id and they surface here as
//!   [`AgentEvent::Subagent`].
//! - Questions: the SDK has no public answer channel for `askQuestion`
//!   (its `request` stream message carries only an id), so the tool is
//!   disallowed at agent creation — a question would otherwise block the
//!   run forever with no way to answer it. Known gap vs the ACP surface.
//! - AUTH: the SDK's credentials are SEPARATE from `cursor-agent login`
//!   (verified live). Runs need `CURSOR_API_KEY` (or a prior SDK browser
//!   login); the shim surfaces the exact fix as an error chip otherwise.
//! - Steering: turn-boundary — steers queue and become the next turn on the
//!   parked session (parity with the previous ACP behavior).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, TodoItem, ToolCall,
};

use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};

mod catalog;

/// The pinned SDK (public beta 1.0.x line; inspected against 1.0.28's
/// typings). Bump deliberately — see the module header.
const CURSOR_SDK_PIN: &str = "@cursor/sdk@1.0.28";
const SHIM_NAME: &str = "komet-cursor-shim.mjs";
const SHIM_SOURCE: &str = include_str!("shim.mjs");

fn cursor_cli_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local").join("bin").join("cursor-agent"));
        dirs.push(home.join(".cursor").join("bin").join("cursor-agent"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/cursor-agent"));
    dirs.push(PathBuf::from("/usr/local/bin/cursor-agent"));
    dirs
}

/// The Cursor harness. Construct with [`CursorHarness::new`]; tests point it
/// at a fake shim process with [`CursorHarness::with_executable`].
pub struct CursorHarness {
    /// Test seam: run this program AS the shim instead of node+managed SDK.
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Discovery cache: only a successful, non-empty catalog is cached, so a
    /// failed probe (offline, SDK churn) retries on the next picker open.
    models_cache: tokio::sync::OnceCell<Vec<Model>>,
}

impl Default for CursorHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            models_cache: tokio::sync::OnceCell::new(),
        }
    }
}

impl CursorHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// `cursor-agent --list-models` is the account's real catalog. Skipped
    /// when a test shim override is in play so fixtures stay deterministic.
    async fn discover_cli_models(&self) -> Result<Vec<Model>, HarnessError> {
        if self.uses_shim_override() {
            return Err(HarnessError::Protocol(
                "shim override skips CLI catalog".into(),
            ));
        }
        let exe = crate::acp::find_on_paths("cursor-agent", cursor_cli_paths())
            .ok_or_else(|| HarnessError::NotInstalled("cursor-agent".into()))?;
        let mut cmd = Command::new(&exe);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.arg("--list-models")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let run = async {
            let output = cmd
                .output()
                .await
                .map_err(|e| HarnessError::Protocol(format!("cursor-agent --list-models: {e}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let models = catalog::fold_cli_models(&stdout);
            if models.is_empty() {
                return Err(HarnessError::Protocol(
                    "cursor-agent --list-models returned no catalog".into(),
                ));
            }
            Ok(models)
        };
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .map_err(|_| HarnessError::Protocol("cursor-agent --list-models timed out".into()))?
    }

    /// Spawn the shim in models mode and map its one catalog frame.
    async fn discover_sdk_models(&self) -> Result<Vec<Model>, HarnessError> {
        let (exe, args) = self.resolve_shim().await?;
        let mut cmd = Command::new(&exe);
        cmd.args(&args);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.arg("models")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let run = async {
            let output = cmd
                .output()
                .await
                .map_err(|e| HarnessError::Protocol(format!("cursor models probe: {e}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let items = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
                .find(|v| v.get("ev").and_then(Value::as_str) == Some("models"))
                .and_then(|v| v.get("items").cloned())
                .ok_or_else(|| {
                    HarnessError::Protocol("cursor models probe returned no catalog".into())
                })?;
            Ok::<_, HarnessError>(catalog::map_model_items(&items))
        };
        tokio::time::timeout(Duration::from_secs(15), run)
            .await
            .map_err(|_| HarnessError::Protocol("cursor models probe timed out".into()))?
    }

    fn uses_shim_override(&self) -> bool {
        self.executable.is_some()
            || std::env::var_os("CURSOR_SDK_SHIM_EXECUTABLE").is_some_and(|p| !p.is_empty())
    }

    /// (program, args) for the shim process: the test override, or node
    /// running the shim inside the managed SDK install (installing it on
    /// first use).
    pub async fn resolve_shim(&self) -> Result<(PathBuf, Vec<String>), HarnessError> {
        if let Some(p) = &self.executable {
            return Ok((p.clone(), Vec::new()));
        }
        if let Some(p) = std::env::var_os("CURSOR_SDK_SHIM_EXECUTABLE")
            && !p.is_empty()
        {
            return Ok((PathBuf::from(p), Vec::new()));
        }
        let pin = crate::adapter_install::NpmPin::parse(CURSOR_SDK_PIN);
        let shim =
            crate::adapter_install::ensure_installed_shim(pin, "Cursor", SHIM_NAME, SHIM_SOURCE)
                .await?;
        crate::adapter_install::launch_for_entry(&shim)
    }
}

/// A ready-to-spawn command for the shim's LOGIN mode: the SDK's PKCE
/// browser flow, minting the key into `store_path` (never the live
/// `~/.cursor/sdk/auth.json` — the engine snapshots the store file as an
/// account slot). Emits `{"ev":"auth-url"}` then `{"ev":"logged-in"}` /
/// `{"ev":"fatal"}` JSONL on stdout; kill to cancel.
pub async fn login_command(store_path: &std::path::Path) -> Result<Command, HarnessError> {
    let (exe, args) = CursorHarness::default().resolve_shim().await?;
    let mut cmd = Command::new(&exe);
    cmd.args(&args);
    crate::compose_child_path(&mut cmd, &exe);
    cmd.arg("login").arg(store_path);
    Ok(cmd)
}

#[async_trait]
impl Harness for CursorHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Cursor
    }
    fn display_name(&self) -> &str {
        "Cursor"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// The SDK has no mid-turn injection; steers queue for the next turn.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    /// "Installed" means the user's own cursor-agent CLI is present — the
    /// user-visible signal they use Cursor (the SDK itself is a managed
    /// install komet performs on demand).
    fn installed(&self) -> bool {
        self.executable.is_some()
            || crate::acp::find_on_paths("cursor-agent", cursor_cli_paths()).is_some()
    }
    /// Done is the SDK run's terminal result, for every turn shape.
    fn deterministic_turn_end(&self) -> bool {
        true
    }

    /// Cursor-native families (Auto / Composer / Grok) from
    /// `cursor-agent --list-models`, then the SDK list, then the snapshot.
    /// New Composer/Grok versions appear when the CLI lists them. A failed
    /// live probe is UNCACHED so the next picker open retries.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        if let Some(models) = self.models_cache.get() {
            return Ok(models.clone());
        }
        if let Ok(models) = self.discover_cli_models().await
            && !models.is_empty()
        {
            let _ = self.models_cache.set(models.clone());
            return Ok(models);
        }
        match self.discover_sdk_models().await {
            Ok(models) if !models.is_empty() => {
                let _ = self.models_cache.set(models.clone());
                Ok(models)
            }
            Ok(_) | Err(_) => Ok(catalog::static_models()),
        }
    }

    /// cursor-agent TUI built-ins (Bucket A). `@cursor/sdk` 1.0.28 still has
    /// no listing API, so the static catalog is the source — same prompt-line
    /// shape as Claude/Codex (`/name` sent as the turn text).
    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        Ok(catalog::static_commands())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (exe, args) = self.resolve_shim().await?;
        let mut cmd = Command::new(&exe);
        cmd.args(&args);
        crate::compose_child_path(&mut cmd, &exe);
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("cursor shim has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("cursor shim has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "komet_harness::cursor", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(stdin_writer(stdin, stdin_rx));
        let first = json!({
            "op": "run",
            "prompt": request.prompt,
            "cwd": request.cwd,
            "model": request
                .model
                .as_deref()
                .map(catalog::sdk_model_id),
            // Typed parameter picks (thinking/context/effort/fast/…) — the
            // shim folds them into the SDK's ModelSelection params.
            "modelOptions": request.model_options,
            "resume": request.resume,
            "sandbox": request.sandbox,
        });
        let _ = stdin_tx.send(first.to_string());

        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            stdout_lines: BufReader::new(stdout).lines(),
            stdin_tx,
            event_tx,
            controls,
            request_cwd: request.cwd,
            request_model: request.model.unwrap_or_default(),
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            tracing::debug!(target: "komet_harness::cursor", "stdin write failed (tolerated): {e}");
            return;
        }
    }
    let _ = stdin.shutdown().await;
}

struct Session {
    child: Child,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin_tx: mpsc::UnboundedSender<String>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request_cwd: String,
    request_model: String,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: crate::StderrTail,
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn run_session(session: Session) {
    let Session {
        mut child,
        mut stdout_lines,
        stdin_tx,
        event_tx,
        controls,
        request_cwd,
        request_model,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input: _request_input,
        request_permission: _request_permission,
        mut steering,
        interrupt,
    } = controls;

    let mut assistant_message_id = new_message_id();
    let mut session_id: Option<String> = None;
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut any_done = false;
    let mut done_after_interrupt = false;
    // A turn is settled and the session is parked awaiting the next prompt.
    let mut parked = false;
    let mut queued_steers: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    let send = |ev: AgentEvent| {
        let tx = event_tx.clone();
        async move { tx.send(Ok(ev)).await.is_ok() }
    };

    'main: loop {
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(frame) = serde_json::from_str::<Value>(line) else {
                        tracing::debug!(target: "komet_harness::cursor", "unparseable shim frame (skipped)");
                        continue;
                    };
                    match frame.get("ev").and_then(Value::as_str).unwrap_or("") {
                        "ready" => {
                            let agent_id = frame
                                .get("agentId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            session_id = Some(agent_id.clone());
                            let model = frame
                                .get("model")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| request_model.clone());
                            if !send(AgentEvent::SessionStarted {
                                harness: HarnessId::Cursor,
                                model,
                                tools: Vec::new(),
                                cwd: request_cwd.clone(),
                                session_id: agent_id,
                                assistant_message_id: assistant_message_id.clone(),
                            })
                            .await
                            {
                                break 'main;
                            }
                        }
                        _ => {
                            for ev in map_shim_frame(&frame, interrupted, &request_model) {
                                let is_done = matches!(ev, AgentEvent::Done { .. });
                                // Stamp the session id onto Dones the mapper
                                // couldn't know.
                                let ev = if let AgentEvent::Done { status, result, error, .. } = ev {
                                    AgentEvent::Done { status, result, error, session_id: session_id.clone(), reason: None }
                                } else {
                                    ev
                                };
                                if !send(ev).await {
                                    break 'main;
                                }
                                if is_done {
                                    any_done = true;
                                    if interrupted {
                                        done_after_interrupt = true;
                                        break 'main;
                                    }
                                    // Turn boundary: a queued steer becomes
                                    // the next turn; otherwise park for the
                                    // mailbox (caller owns teardown).
                                    if let Some(text) = queued_steers.pop_front() {
                                        let prev = std::mem::replace(
                                            &mut assistant_message_id,
                                            new_message_id(),
                                        );
                                        if !send(AgentEvent::Steered {
                                            assistant_message_id: Some(prev),
                                            next_assistant_message_id: Some(
                                                assistant_message_id.clone(),
                                            ),
                                        })
                                        .await
                                        {
                                            break 'main;
                                        }
                                        let _ = stdin_tx.send(
                                            json!({ "op": "user", "prompt": text }).to_string(),
                                        );
                                    } else if !steering_open {
                                        break 'main;
                                    } else {
                                        parked = true;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(None) => break 'main, // shim exited
                Err(e) => {
                    let _ = event_tx.send(Err(HarnessError::Io(e))).await;
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    if parked {
                        parked = false;
                        let prev = std::mem::replace(&mut assistant_message_id, new_message_id());
                        if !send(AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(assistant_message_id.clone()),
                        })
                        .await
                        {
                            break 'main;
                        }
                        let _ = stdin_tx
                            .send(json!({ "op": "user", "prompt": msg.prompt }).to_string());
                    } else {
                        // Turn-boundary steering: queue for the next boundary.
                        queued_steers.push_back(msg.prompt);
                    }
                }
                None => {
                    steering_open = false;
                    if parked && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                let _ = stdin_tx.send(json!({ "op": "interrupt" }).to_string());
                if let Some(pid) = child.id() {
                    escalation = Some(tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        send_signal(pid, Signal::Term);
                        tokio::time::sleep(kill_grace).await;
                        send_signal(pid, Signal::Kill);
                    }));
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: session_id.clone(),
                    reason: None,
                }))
                .await;
        } else if !interrupted && !any_done {
            // Give the just-died child a beat to be reaped and its stderr
            // reader to drain, so the crash message carries the real exit
            // status and tail instead of "still running".
            let status = tokio::time::timeout(Duration::from_millis(500), child.wait())
                .await
                .ok()
                .and_then(Result::ok);
            tokio::task::yield_now().await;
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("cursor shim", status, &stderr_tail)),
                    session_id: session_id.clone(),
                    reason: None,
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Decode one cursor SDK tool (public vocabulary name + args) into a typed
/// [`ToolCall`], tolerant of arg spellings.
fn decode_tool(name: &str, args: &Value) -> ToolCall {
    let s = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .to_owned()
    };
    let opt = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
            .map(str::to_owned)
    };
    match name {
        "shell" => ToolCall::Exec {
            command: s(&["command"]),
        },
        "read" => ToolCall::ReadFile {
            path: s(&["path", "filePath", "file_path"]),
        },
        "write" => ToolCall::WriteFile {
            path: s(&["path", "filePath", "file_path"]),
            content: None,
        },
        "edit" => ToolCall::EditFile {
            path: s(&["path", "filePath", "file_path"]),
            old_string: None,
            new_string: None,
        },
        "delete" => ToolCall::Unknown {
            name: format!("Delete: {}", s(&["path", "filePath", "file_path"])),
            input: (!args.is_null()).then(|| args.clone()),
        },
        "grep" => ToolCall::Search {
            pattern: s(&["pattern", "query"]),
            path: opt(&["path", "directory"]),
        },
        "glob" => ToolCall::Glob {
            pattern: s(&["pattern", "globPattern"]),
        },
        "webSearch" => ToolCall::WebSearch {
            query: s(&["query", "search"]),
        },
        "webFetch" => ToolCall::WebFetch {
            url: s(&["url"]),
            prompt: None,
        },
        "updateTodos" => ToolCall::Todo {
            items: args
                .get("todos")
                .or_else(|| args.get("items"))
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: t
                        .get("content")
                        .or_else(|| t.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    done: t.get("status").and_then(Value::as_str) == Some("completed")
                        || t.get("completed").and_then(Value::as_bool) == Some(true),
                })
                .collect(),
        },
        "mcp" => ToolCall::Mcp {
            server: s(&["server", "serverName"]),
            tool: s(&["tool", "toolName", "name"]),
            input: args.get("args").or(args.get("input")).cloned(),
        },
        // The subagent spawn: the chip the engine folds; its interior
        // arrives as tagged frames keyed by this call's id.
        "task" => ToolCall::Unknown {
            name: {
                let description = s(&["description", "prompt"]);
                if description.is_empty() {
                    "Agent".to_owned()
                } else {
                    format!("Agent: {description}")
                }
            },
            input: (!args.is_null()).then(|| args.clone()),
        },
        other => ToolCall::Unknown {
            name: other.to_owned(),
            input: (!args.is_null()).then(|| args.clone()),
        },
    }
}

/// Map one shim frame to events. `Done` session ids are stamped by the loop.
fn map_shim_frame(frame: &Value, interrupted: bool, model: &str) -> Vec<AgentEvent> {
    let text = || {
        frame
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let parent = frame
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tag = |ev: AgentEvent| -> AgentEvent {
        match &parent {
            Some(parent) => AgentEvent::Subagent {
                parent_tool_use_id: parent.clone(),
                event: Box::new(ev),
            },
            None => ev,
        }
    };
    match frame.get("ev").and_then(Value::as_str).unwrap_or("") {
        "text" => vec![tag(AgentEvent::TextDelta { text: text() })],
        "thinking" => vec![tag(AgentEvent::ReasoningDelta { text: text() })],
        "tool" => {
            let id = frame
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let name = frame.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = frame.get("args").cloned().unwrap_or(Value::Null);
            match frame.get("phase").and_then(Value::as_str) {
                Some("start") => {
                    let mut events = vec![tag(AgentEvent::ToolCall {
                        id: id.clone(),
                        call: decode_tool(name, &args),
                    })];
                    // A spawn's prompt is the subagent's opening user
                    // message — the SDK's nested stream never carries it
                    // (only the child's own updates), so seed it at the
                    // spawn and the subagent transcript starts the way
                    // every chat does. Top-level spawns only: nested
                    // spawns' interiors share their parent's doc.
                    if name == "task"
                        && parent.is_none()
                        && let Some(prompt) = args
                            .get("prompt")
                            .or_else(|| args.get("description"))
                            .and_then(Value::as_str)
                            .filter(|p| !p.trim().is_empty())
                    {
                        events.push(AgentEvent::Subagent {
                            parent_tool_use_id: id,
                            event: Box::new(AgentEvent::UserMessage {
                                text: prompt.to_owned(),
                            }),
                        });
                    }
                    events
                }
                Some("end") => {
                    let is_error = frame.get("error").and_then(Value::as_bool) == Some(true);
                    let mut events = vec![
                        tag(AgentEvent::ToolCall {
                            id: id.clone(),
                            call: decode_tool(name, &args),
                        }),
                        tag(AgentEvent::ToolResult {
                            id: id.clone(),
                            is_error,
                            output: None,
                            diff: None,
                        }),
                    ];
                    // A finished task IS the subagent finishing: the SDK has
                    // no separate terminal frame for the nested transcript,
                    // so the spawn tool's end doubles as the tagged Done that
                    // flips the chip and freezes the subagent doc.
                    if name == "task" && parent.is_none() {
                        events.push(AgentEvent::Subagent {
                            parent_tool_use_id: id,
                            event: Box::new(AgentEvent::Done {
                                status: if is_error {
                                    DoneStatus::Errored
                                } else {
                                    DoneStatus::Completed
                                },
                                result: None,
                                error: None,
                                session_id: None,
                                reason: None,
                            }),
                        });
                    }
                    events
                }
                _ => Vec::new(),
            }
        }
        "usage" => {
            let input = frame.get("input").and_then(Value::as_u64).unwrap_or(0);
            let output = frame.get("output").and_then(Value::as_u64).unwrap_or(0);
            let limit = komet_proto::default_context_limit_for_model(model);
            vec![
                AgentEvent::Usage {
                    input_tokens: input,
                    cached_input_tokens: 0,
                    output_tokens: output,
                    reasoning_tokens: 0,
                    context_limit: Some(limit),
                },
                AgentEvent::ContextWindow {
                    stats: komet_proto::ContextUsageStats::window(
                        input,
                        limit,
                        komet_proto::ContextUsageSource::Approximate,
                        input,
                        0,
                        output,
                        0,
                    ),
                },
            ]
        }
        "turn" => {
            let status = frame.get("status").and_then(Value::as_str).unwrap_or("");
            let error = frame
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let status = if interrupted || status == "cancelled" {
                DoneStatus::Interrupted
            } else if status == "error" {
                DoneStatus::Errored
            } else {
                DoneStatus::Completed
            };
            vec![AgentEvent::Done {
                status,
                result: None,
                error,
                session_id: None,
                reason: None,
            }]
        }
        "fatal" => vec![AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some(
                frame
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("cursor shim failed")
                    .to_owned(),
            ),
            session_id: None,
            reason: None,
        }],
        other => {
            tracing::debug!(target: "komet_harness::cursor", "unknown shim frame (skipped): {other}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_cursor_tool_vocabulary() {
        assert_eq!(
            decode_tool("shell", &json!({"command": "ls"})),
            ToolCall::Exec {
                command: "ls".into()
            }
        );
        assert_eq!(
            decode_tool("grep", &json!({"pattern": "foo", "path": "/src"})),
            ToolCall::Search {
                pattern: "foo".into(),
                path: Some("/src".into())
            }
        );
        assert!(matches!(
            decode_tool("task", &json!({"description": "map the repo"})),
            ToolCall::Unknown { name, .. } if name == "Agent: map the repo"
        ));
        assert!(matches!(
            decode_tool("somethingNew", &json!({})),
            ToolCall::Unknown { .. }
        ));
    }

    #[test]
    fn task_start_seeds_the_subagent_opening_user_message() {
        let frame: Value = serde_json::from_str(
            r#"{"ev":"tool","phase":"start","id":"call_task_1","name":"task","args":{"description":"probe","prompt":"scan the fold path"}}"#,
        )
        .unwrap();
        let events = map_shim_frame(&frame, false, "");
        assert!(matches!(
            &events[..],
            [
                AgentEvent::ToolCall { id, .. },
                AgentEvent::Subagent { parent_tool_use_id, event },
            ] if id == "call_task_1"
                && parent_tool_use_id == "call_task_1"
                && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "scan the fold path")
        ));
        // A NESTED spawn's interior shares its parent's doc: no seeding.
        let nested: Value = serde_json::from_str(
            r#"{"ev":"tool","phase":"start","id":"call_task_2","name":"task","args":{"prompt":"inner"},"parent":"call_task_1"}"#,
        )
        .unwrap();
        assert_eq!(map_shim_frame(&nested, false, "").len(), 1);
    }

    #[test]
    fn usage_overshoot_is_capped_to_the_model_window() {
        let events = map_shim_frame(
            &json!({"ev":"usage","input":3_000_000,"output":18_000}),
            false,
            "grok-4.6",
        );
        let Some(AgentEvent::ContextWindow { stats }) = events
            .iter()
            .find(|e| matches!(e, AgentEvent::ContextWindow { .. }))
        else {
            panic!("expected ContextWindow: {events:?}");
        };
        let limit = komet_proto::default_context_limit_for_model("grok-4.6");
        assert_eq!(limit, 131_072);
        assert_eq!(stats.context_limit, limit);
        assert_eq!(stats.used(), limit);
        assert_eq!(stats.input_tokens, 3_000_000);
        assert_eq!(stats.source, komet_proto::ContextUsageSource::Approximate);
    }

    #[test]
    fn nested_frames_arrive_tagged() {
        let frame: Value =
            serde_json::from_str(r#"{"ev":"text","text":"sub says","parent":"call_task_1"}"#)
                .unwrap();
        assert_eq!(
            map_shim_frame(&frame, false, ""),
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "call_task_1".into(),
                event: Box::new(AgentEvent::TextDelta {
                    text: "sub says".into()
                }),
            }]
        );
    }
}
