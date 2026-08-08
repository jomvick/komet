//! ACP harness: spawns an Agent Client Protocol agent (JSON-RPC 2.0 over
//! stdio, protocol v1) and maps its session updates onto [`AgentEvent`]s. One
//! implementation covers every ACP agent; [`AcpHarness::grok`] configures it
//! for xAI's Grok Build (`grok agent stdio`), the first registered agent.
//!
//! - `initialize` (protocolVersion 1, fs/terminal capabilities declined) →
//!   `session/new`, or `session/load` with a fresh-session fallback when
//!   resuming; replayed history during a load is dropped (the doc already
//!   holds it).
//! - `session/prompt` owns the turn: its response's `stopReason` ends the
//!   turn (`cancelled` → Interrupted, `refusal` → Errored, else Completed).
//! - `session/update` notifications normalize per [`normalize::map_update`]:
//!   message/thought chunks, tool calls with capped inline output + diffs,
//!   plans → Todo, `available_commands_update` → [`AgentEvent::AvailableCommands`].
//! - Permission requests are auto-accepted with the agent's preferred allow
//!   option — parity with the claude/codex harnesses' unattended yolo mode.
//! - Steering: agents advertising the `_session/steering` extension
//!   (`initialize._meta.steering.supported`) get mid-turn injection; others
//!   (Grok today) queue steers and deliver them as the next `session/prompt`
//!   at the turn boundary. The session stays parked between turns while the
//!   steering mailbox lives, like the codex harness.
//! - Interrupt: `session/cancel`, escalating SIGTERM → SIGKILL; the stream
//!   always ends with `Done { status: Interrupted }`.

mod normalize;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};
use normalize::{map_update, parse_commands, preferred_allow_option};

/// Per-agent configuration: which binary to spawn and what to tell the picker.
struct AcpAgentSpec {
    id: HarnessId,
    display_name: &'static str,
    /// Binary name searched on PATH (and platform install dirs).
    executable: &'static str,
    /// Env var overriding executable resolution (tests, custom installs).
    env_override: &'static str,
    /// Arguments that put the binary in ACP-serving mode.
    args: &'static [&'static str],
    /// Extra install locations to probe after PATH.
    extra_paths: fn() -> Vec<PathBuf>,
    /// Search summary + install hint for the NotInstalled error.
    install_hint: &'static str,
    models: fn() -> Vec<Model>,
    steering_mode: SteeringMode,
    /// Effort ladder surfaced in the picker; applied per session via the
    /// `thought_level` config option (must mirror the registry descriptor).
    reasoning_levels: &'static [ReasoningLevel],
}

fn grok_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Grok,
        display_name: "Grok",
        executable: "grok",
        env_override: "GROK_EXECUTABLE",
        args: &["agent", "stdio"],
        extra_paths: || {
            let mut dirs = Vec::new();
            if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                dirs.push(home.join(".local").join("bin").join("grok"));
                dirs.push(home.join(".grok").join("bin").join("grok"));
                dirs.push(home.join(".npm-global").join("bin").join("grok"));
            }
            dirs.push(PathBuf::from("/opt/homebrew/bin/grok"));
            dirs.push(PathBuf::from("/usr/local/bin/grok"));
            dirs
        },
        install_hint: "grok (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.grok/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, and \
             fnm/nvm/volta/pnpm/bun install dirs; install with \
             `curl -fsSL https://x.ai/cli/install.sh | bash` or \
             `npm install -g @xai-official/grok`; set GROK_EXECUTABLE to override)",
        models: || {
            vec![Model {
                id: "grok-4.5".into(),
                label: "Grok 4.5".into(),
                description: Some("xAI's coding model — 500k context".into()),
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ],
                options: Vec::new(),
            }]
        },
        // No `_session/steering` extension: steers deliver at turn boundaries.
        steering_mode: SteeringMode::TurnBoundary,
        // Grok Build's advertised efforts (default high); applied through the
        // session's `thought_level` config option.
        reasoning_levels: &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ],
    }
}

/// The ACP harness. Construct with [`AcpHarness::grok`]; tests point it at a
/// fake agent with [`AcpHarness::with_executable`].
pub struct AcpHarness {
    spec: AcpAgentSpec,
    executable: Option<PathBuf>,
    /// Grace between `session/cancel` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Discovery result cache: the advertised commands survive across calls.
    commands: tokio::sync::OnceCell<Vec<SlashCommand>>,
}

impl AcpHarness {
    /// Grok Build (`grok agent stdio`) — xAI's native ACP agent.
    pub fn grok() -> Self {
        Self {
            spec: grok_spec(),
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            commands: tokio::sync::OnceCell::new(),
        }
    }

    /// Use a fixed agent binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        if let Some(p) = std::env::var_os(self.spec.env_override)
            && !p.is_empty()
        {
            return Ok(PathBuf::from(p));
        }
        let exe = self.spec.executable;
        let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.join(exe))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(shell_path) = crate::shell_env::login_shell_path() {
            candidates.extend(
                std::env::split_paths(shell_path)
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.join(exe)),
            );
        }
        candidates.extend((self.spec.extra_paths)());
        candidates.extend(
            crate::node_version_manager_bins()
                .into_iter()
                .map(|d| d.join(exe)),
        );
        candidates
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| HarnessError::NotInstalled(self.spec.install_hint.into()))
    }

    fn spawn_agent(&self, cwd: Option<&str>) -> Result<(Child, crate::StderrTail), HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        cmd.args(self.spec.args);
        crate::compose_child_path(&mut cmd, &exe);
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            cmd.current_dir(cwd);
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
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "komet_harness::acp", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }
        Ok((child, stderr_tail))
    }

    /// Short-lived discovery run for [`Harness::commands`]: initialize, scan
    /// the response, then try one unauthenticated `session/new` and wait
    /// briefly for `available_commands_update`. Best-effort — an agent that
    /// refuses sessions before login still surfaces whatever the handshake
    /// advertised.
    async fn discover_commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let (mut child, _stderr) = self.spawn_agent(None)?;
        let (client, mut incoming) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => RpcClient::new(stdin, stdout),
            _ => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(HarnessError::Protocol("agent child has no stdio".into()));
            }
        };
        let discovery = async {
            let init = client.request("initialize", initialize_params()).await?;
            let mut commands = scan_available_commands(&init);
            if commands.is_empty() {
                let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                let session = client
                    .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                    .await;
                if session.is_ok() {
                    // The update usually arrives within milliseconds of the
                    // session response; 2s bounds a quiet agent.
                    let deadline = tokio::time::sleep(Duration::from_secs(2));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            inc = incoming.recv() => match inc {
                                Some(Incoming::Notification { method, params })
                                    if method == "session/update" =>
                                {
                                    let update = params.get("update").cloned().unwrap_or(Value::Null);
                                    if update.get("sessionUpdate").and_then(Value::as_str)
                                        == Some("available_commands_update")
                                    {
                                        commands = parse_commands(update.get("availableCommands"));
                                        break;
                                    }
                                }
                                Some(Incoming::Request { id, .. }) => {
                                    client.respond_error(&id, -32601, "unsupported during discovery");
                                }
                                Some(_) => {}
                                None => break,
                            },
                            _ = &mut deadline => break,
                        }
                    }
                }
            }
            Ok::<Vec<SlashCommand>, HarnessError>(commands)
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("command discovery timed out".into())),
        }
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> HarnessId {
        self.spec.id
    }
    fn display_name(&self) -> &str {
        self.spec.display_name
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        self.spec.steering_mode
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        self.spec.reasoning_levels
    }

    /// Static catalog; an absent binary surfaces as NotInstalled here, like
    /// the codex harness.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        Ok((self.spec.models)())
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands
            .get_or_try_init(|| self.discover_commands())
            .await
            .cloned()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (mut child, stderr_tail) = self.spawn_agent(Some(&request.cwd))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("agent child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("agent child has no stdout".into()))?;
        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            harness: self.spec.id,
            agent_name: self.spec.display_name,
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

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    harness: HarnessId,
    agent_name: &'static str,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: crate::StderrTail,
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientInfo": {
            "name": "komet-native",
            "title": "Komet",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // Declined: agents fall back to their own fs/terminal access, which
        // is what komet wants — the working tree is the source of truth for
        // the diff pane, and commands belong to the agent's own sandbox.
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        },
    })
}

/// `initialize._meta.steering.supported` — the `_session/steering` extension
/// both org-maintained adapters advertise (not part of the v1 spec).
fn steering_supported(init: &Value) -> bool {
    init.get("_meta")
        .and_then(|m| m.get("steering"))
        .and_then(|s| s.get("supported"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Depth-limited scan for an `availableCommands` array anywhere in a response
/// (agents differ on where the handshake advertises them: top level, inside
/// `agentCapabilities`, or `_meta`).
fn scan_available_commands(value: &Value) -> Vec<SlashCommand> {
    fn scan(value: &Value, depth: u8) -> Option<&Value> {
        if depth == 0 {
            return None;
        }
        let obj = value.as_object()?;
        if let Some(cmds) = obj.get("availableCommands").filter(|c| c.is_array()) {
            return Some(cmds);
        }
        obj.values().find_map(|v| scan(v, depth - 1))
    }
    parse_commands(scan(value, 4))
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// The `session/set_config_option` calls a session response's `configOptions`
/// warrant for this run: the requested model (category `model`) and effort
/// (category `thought_level`), matched against the option's advertised value
/// ids and skipped when already current. Pure so it's testable; unknown
/// categories and boolean options are left alone.
fn config_option_sets(
    session_response: &Value,
    model: Option<&str>,
    reasoning: Option<ReasoningLevel>,
) -> Vec<(String, String)> {
    let Some(options) = session_response
        .get("configOptions")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut sets = Vec::new();
    for option in options {
        if option.get("type").and_then(Value::as_str) != Some("select") {
            continue;
        }
        let (Some(config_id), Some(category)) = (
            option.get("id").and_then(Value::as_str),
            option.get("category").and_then(Value::as_str),
        ) else {
            continue;
        };
        let current = option.get("currentValue").and_then(Value::as_str);
        let available: Vec<&str> = option
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.get("value").and_then(Value::as_str))
            .collect();
        let wanted: Option<&str> = match category {
            "model" => model.filter(|m| available.contains(m)),
            "thought_level" => reasoning.and_then(|level| {
                // Preference ladder per komet level; the first value the
                // agent actually advertises wins (Grok: low/medium/high).
                let candidates: &[&str] = match level {
                    ReasoningLevel::Minimal => &["minimal", "low"],
                    ReasoningLevel::Low => &["low", "minimal"],
                    ReasoningLevel::Medium => &["medium"],
                    ReasoningLevel::High => &["high"],
                    ReasoningLevel::XHigh => &["xhigh", "x-high", "high"],
                    ReasoningLevel::Max => &["max", "xhigh", "high"],
                    ReasoningLevel::Ultra
                    | ReasoningLevel::Ultracode
                    | ReasoningLevel::Ultrathink => &["ultra", "max", "high"],
                };
                candidates.iter().find(|c| available.contains(*c)).copied()
            }),
            _ => None,
        };
        if let Some(value) = wanted
            && current != Some(value)
        {
            sets.push((config_id.to_owned(), value.to_owned()));
        }
    }
    sets
}

/// The events of one `session/update` notification, session-filtered.
fn session_update_events(params: &Value, session_id: &str) -> Vec<AgentEvent> {
    if params.get("sessionId").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    map_update(params.get("update").unwrap_or(&Value::Null))
}

/// Map a finished `session/prompt` result to the run's terminal status.
fn stop_outcome(
    res: &Result<Value, HarnessError>,
    interrupted: bool,
) -> (DoneStatus, Option<String>) {
    if interrupted {
        return (DoneStatus::Interrupted, None);
    }
    match res {
        Ok(resp) => match resp.get("stopReason").and_then(Value::as_str) {
            Some("cancelled") => (DoneStatus::Interrupted, None),
            Some("refusal") => (
                DoneStatus::Errored,
                Some("The agent refused to continue.".to_owned()),
            ),
            // end_turn / max_tokens / max_turn_requests: the turn ended;
            // partial output is already in the doc.
            _ => (DoneStatus::Completed, None),
        },
        Err(e) => (DoneStatus::Errored, Some(e.to_string())),
    }
}

/// One turn: `session/prompt` whose response (the `stopReason`) ends it.
fn prompt_turn(
    client: RpcClient,
    session_id: String,
    text: String,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    Box::pin(async move {
        client
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await
    })
}

/// Answer a server→client request. Permission requests are auto-accepted with
/// the agent's preferred allow option — parity with the claude harness's
/// bypassPermissions and the codex harness's approvalPolicy "never" (komet
/// sessions run unattended). Everything else (fs, terminal, elicitation) was
/// declined at initialize, so a stray request gets method-not-found rather
/// than wedging the agent.
fn handle_server_request(client: &RpcClient, id: Value, method: &str, params: &Value) {
    if method != "session/request_permission" {
        tracing::debug!(target: "komet_harness::acp", "unhandled server request: {method}");
        client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
        return;
    }
    let options: Vec<Value> = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match preferred_allow_option(&options) {
        Some(option_id) => client.respond(
            &id,
            json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
        ),
        None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
    }
}

/// Await a setup request while draining incoming messages, so a `session/load`
/// whose replay outruns the incoming channel's capacity can't deadlock the
/// reader. Replayed `session/update`s are dropped (the doc already holds the
/// history); server requests are answered.
async fn request_draining(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    method: &'static str,
    params: Value,
) -> Result<Value, HarnessError> {
    let mut fut = prompt_like_request(client.clone(), method, params);
    let res = loop {
        tokio::select! {
            res = &mut fut => break res,
            inc = incoming.recv() => match inc {
                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(client, id, &method, &params);
                }
                Some(_) => {}
                None => {
                    return Err(HarnessError::Protocol(format!(
                        "{method}: agent exited during setup"
                    )));
                }
            },
        }
    };
    // Responses resolve through the pending map, not the incoming queue, so
    // replay updates the reader forwarded BEFORE the response line may still
    // sit in the buffer — flush them now or they'd leak into the live turn.
    while let Ok(inc) = incoming.try_recv() {
        if let Incoming::Request { id, method, params } = inc {
            handle_server_request(client, id, &method, &params);
        }
    }
    res
}

fn prompt_like_request(
    client: RpcClient,
    method: &'static str,
    params: Value,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    Box::pin(async move { client.request(method, params).await })
}

/// The per-run event loop: one task multiplexing agent messages, the pending
/// turn, the steering mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        harness,
        agent_name,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input: _request_input,
        mut steering,
        interrupt,
    } = controls;

    // ---- handshake + session (interruptible) ------------------------------
    let setup = async {
        let init = client.request("initialize", initialize_params()).await?;
        let steer_ext = steering_supported(&init);
        let init_commands = scan_available_commands(&init);

        let session_params = json!({ "cwd": request.cwd, "mcpServers": [] });
        let (session_id, session_response) = if let Some(resume) = &request.resume {
            let mut load = session_params.clone();
            load["sessionId"] = Value::String(resume.clone());
            match request_draining(&client, &mut incoming, "session/load", load).await {
                Ok(resp) => (resume.clone(), resp),
                // A missing/foreign session falls back to a fresh one.
                Err(e) => {
                    tracing::debug!(
                        target: "komet_harness::acp",
                        "session/load failed (starting fresh): {e}"
                    );
                    let new = request_draining(
                        &client,
                        &mut incoming,
                        "session/new",
                        session_params.clone(),
                    )
                    .await?;
                    (
                        new.get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        new,
                    )
                }
            }
        } else {
            let new =
                request_draining(&client, &mut incoming, "session/new", session_params).await?;
            (
                new.get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                new,
            )
        };
        if session_id.is_empty() {
            return Err(HarnessError::Protocol(
                "session/new returned no sessionId".into(),
            ));
        }
        // Apply the run's model + effort through the session's advertised
        // config options (ACP has no per-prompt model field). Best-effort:
        // a rejected set is logged, never fatal — the agent's default runs.
        for (config_id, value) in config_option_sets(
            &session_response,
            request.model.as_deref(),
            request.reasoning,
        ) {
            let params = json!({
                "sessionId": session_id,
                "configId": config_id,
                "value": value,
            });
            if let Err(e) =
                request_draining(&client, &mut incoming, "session/set_config_option", params).await
            {
                tracing::debug!(
                    target: "komet_harness::acp",
                    "session/set_config_option {config_id}={value} rejected (agent default runs): {e}"
                );
            }
        }
        Ok::<(String, bool, Vec<SlashCommand>), HarnessError>((
            session_id,
            steer_ext,
            init_commands,
        ))
    };
    let (session_id, steer_ext, init_commands) = tokio::select! {
        res = setup => match res {
            Ok(v) => v,
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }
    if !init_commands.is_empty()
        && !send(
            &event_tx,
            AgentEvent::AvailableCommands {
                commands: init_commands,
            },
        )
        .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    // ---- main loop --------------------------------------------------------
    let mut turn: Option<BoxFuture<'static, Result<Value, HarnessError>>> = Some(prompt_turn(
        client.clone(),
        session_id.clone(),
        request.prompt.clone(),
    ));
    // Steers waiting for the turn boundary (agents without the extension, or
    // extension steers that lost the turn-end race).
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            res = async { turn.as_mut().expect("guarded by if").await }, if turn.is_some() => {
                turn = None;
                // Updates streamed before the prompt response are already
                // queued in stdout order — fold them into the turn before
                // closing it (responses bypass the incoming queue).
                let mut consumer_gone = false;
                while let Ok(inc) = incoming.try_recv() {
                    match inc {
                        Incoming::Notification { method, params }
                            if method == "session/update" =>
                        {
                            for ev in session_update_events(&params, &session_id) {
                                if !send(&event_tx, ev).await {
                                    consumer_gone = true;
                                    break;
                                }
                            }
                        }
                        Incoming::Request { id, method, params } => {
                            handle_server_request(&client, id, &method, &params);
                        }
                        _ => {}
                    }
                    if consumer_gone {
                        break;
                    }
                }
                if consumer_gone {
                    break 'main;
                }
                let (prev, _next) = rotate(&mut assistant_message_id);
                if !send(
                    &event_tx,
                    AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                )
                .await
                {
                    break 'main;
                }
                let (status, error) = stop_outcome(&res, interrupted);
                done_current = true;
                if interrupted {
                    done_after_interrupt = true;
                }
                if !send(
                    &event_tx,
                    AgentEvent::Done {
                        status,
                        result: None,
                        error,
                        session_id: Some(session_id.clone()),
                    },
                )
                .await
                {
                    break 'main;
                }
                if interrupted || res.is_err() {
                    break 'main;
                }
                // Persistent session: a queued steer becomes the next turn;
                // otherwise stay alive for the mailbox — the caller owns
                // teardown (mirrors the codex harness).
                if let Some(text) = queued_steers.pop_front() {
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                    done_current = false;
                    turn = Some(prompt_turn(client.clone(), session_id.clone(), text));
                } else if !steering_open {
                    break 'main;
                }
            },

            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    if method == "session/update" {
                        for ev in session_update_events(&params, &session_id) {
                            if !send(&event_tx, ev).await {
                                break 'main;
                            }
                        }
                    }
                    // Other notifications (other sessions, agent noise) are
                    // tolerated by design.
                }
                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(&client, id, &method, &params);
                }
                Some(Incoming::Eof) | None => {
                    // The turn ends via a request RESPONSE, which races EOF
                    // through a different channel than notifications: an agent
                    // exiting right after its final response must read as a
                    // clean finish, not a crash. The response (if any) is
                    // already resolved by the reader before it sends Eof.
                    // Only a RESOLVED response is a clean finish; a request
                    // failed by the reader's EOF cleanup falls through to the
                    // crash-message bookkeeping below (stderr tail intact).
                    if let Some(mut fut) = turn.take()
                        && let Ok(res @ Ok(_)) =
                            tokio::time::timeout(Duration::from_millis(50), &mut fut).await
                    {
                        let (prev, _next) = rotate(&mut assistant_message_id);
                        let _ = send(
                            &event_tx,
                            AgentEvent::AssistantMessageCompleted { assistant_message_id: prev },
                        )
                        .await;
                        let (status, error) = stop_outcome(&res, interrupted);
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(session_id.clone()),
                            },
                        )
                        .await;
                    }
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    if turn.is_none() {
                        // Idle between turns: a steer is simply the next turn.
                        let (prev, next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::Steered {
                                assistant_message_id: Some(prev),
                                next_assistant_message_id: Some(next),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        done_current = false;
                        turn = Some(prompt_turn(client.clone(), session_id.clone(), text));
                    } else if steer_ext {
                        // Mid-turn injection via the `_session/steering`
                        // extension. `idleBehavior: promptRequired` covers the
                        // turn-ended race: the agent hands the text back
                        // instead of firing an untracked turn.
                        let params = json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": text }],
                            "_meta": { "steering": { "idleBehavior": "promptRequired" } },
                        });
                        match client.request("_session/steering", params).await {
                            Ok(resp) => {
                                let outcome = resp
                                    .get("outcome")
                                    .and_then(Value::as_str)
                                    .unwrap_or("injected");
                                if outcome == "promptRequired" {
                                    // Raced the turn end: redeliver at the
                                    // boundary the loop is about to hit.
                                    queued_steers.push_back(text);
                                } else {
                                    let (prev, next) = rotate(&mut assistant_message_id);
                                    if !send(
                                        &event_tx,
                                        AgentEvent::Steered {
                                            assistant_message_id: Some(prev),
                                            next_assistant_message_id: Some(next),
                                        },
                                    )
                                    .await
                                    {
                                        break 'main;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    target: "komet_harness::acp",
                                    "_session/steering failed (queued for turn boundary): {e}"
                                );
                                queued_steers.push_back(text);
                            }
                        }
                    } else {
                        // No extension (Grok today): turn-boundary delivery.
                        queued_steers.push_back(text);
                    }
                }
                None => {
                    steering_open = false;
                    if turn.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if turn.is_some() {
                    client.notify("session/cancel", Some(json!({ "sessionId": session_id })));
                    // Escalate if the agent doesn't wind down (stopReason
                    // "cancelled") within the grace periods.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to cancel — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child killed mid-turn must not read as a silent success.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message(agent_name, status, &stderr_tail)),
                    session_id: Some(session_id.clone()),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_capability_reads_initialize_meta() {
        assert!(steering_supported(&json!({
            "protocolVersion": 1,
            "_meta": { "steering": { "supported": true } },
        })));
        assert!(!steering_supported(&json!({ "protocolVersion": 1 })));
        assert!(!steering_supported(&json!({
            "_meta": { "steering": { "supported": false } },
        })));
    }

    #[test]
    fn config_option_sets_map_model_and_effort() {
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "grok-4-fast",
                    "options": [
                        { "value": "grok-4-fast", "name": "Grok 4 Fast" },
                        { "value": "grok-4.5", "name": "Grok 4.5" },
                    ],
                },
                {
                    "id": "effort",
                    "name": "Reasoning effort",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "high",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                    ],
                },
                {
                    "id": "voice",
                    "name": "Voice mode",
                    "category": "other_thing",
                    "type": "boolean",
                    "currentValue": false,
                },
            ],
        });
        // Model needs switching; medium effort differs from current high.
        assert_eq!(
            config_option_sets(&response, Some("grok-4.5"), Some(ReasoningLevel::Medium)),
            vec![
                ("model".to_owned(), "grok-4.5".to_owned()),
                ("effort".to_owned(), "medium".to_owned()),
            ]
        );
        // Already-current values and unadvertised models set nothing.
        assert_eq!(
            config_option_sets(&response, Some("grok-4-fast"), Some(ReasoningLevel::High)),
            Vec::<(String, String)>::new()
        );
        assert_eq!(
            config_option_sets(&response, Some("gpt-5.6-sol"), None),
            Vec::<(String, String)>::new()
        );
        // Unknown komet levels degrade down the preference ladder.
        assert_eq!(
            config_option_sets(&response, None, Some(ReasoningLevel::Ultra)),
            Vec::<(String, String)>::new(), // ultra → high == current
        );
        assert_eq!(
            config_option_sets(&response, None, Some(ReasoningLevel::Minimal)),
            vec![("effort".to_owned(), "low".to_owned())]
        );
        // No configOptions advertised → nothing to set.
        assert_eq!(
            config_option_sets(&json!({"sessionId": "s"}), Some("grok-4.5"), None),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn command_scan_finds_nested_advertisements() {
        let init = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "_meta": {
                    "availableCommands": [
                        { "name": "compact", "description": "Compact the session" },
                    ],
                },
            },
        });
        let commands = scan_available_commands(&init);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "compact");
        assert!(scan_available_commands(&json!({ "protocolVersion": 1 })).is_empty());
    }
}
