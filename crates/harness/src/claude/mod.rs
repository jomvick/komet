//! Claude Code harness: spawns the installed `claude` CLI and speaks its
//! stream-json protocol directly — no adapter process in between. Resurrected
//! from the pre-ACP driver (see docs/research/harness.md) and modernized
//! against CLI 2.1.228.
//!
//! - stdout JSONL frames are normalized into [`AgentEvent`]s (init dedupe,
//!   subagent tagging, typed tool decoding, error-code mapping).
//! - PERMISSIONS ride the stdio control channel: `--permission-prompt-tool
//!   stdio` (undocumented — absent from `claude --help`, but it is the same
//!   transport the Claude Agent SDK's `query()` drives, and was re-validated
//!   live against 2.1.228: `can_use_tool` control requests arrive and
//!   allow/deny responses are honored). The alternative channel — an MCP
//!   permission tool — needs a server process and was rejected. Tool calls
//!   auto-allow (komet sessions run unattended, parity with the ACP
//!   harness's preferred-allow behavior); `AskUserQuestion` round-trips
//!   through [`RunControls::request_input`].
//! - DONE is the CLI's own `result` frame, eagerly: background work (a
//!   spawned subagent) never holds the turn. The CLI natively runs a second
//!   wake turn when a background task finishes — a fresh `init` (same
//!   session id, deduped) plus another `result` — and both are forwarded;
//!   the engine's parked-session resume path turns them into the
//!   done→Working→done wake.
//! - SUBAGENT frames arrive on the same stdout tagged with a top-level
//!   `parent_tool_use_id`; they are wrapped in [`AgentEvent::Subagent`] and
//!   NEVER folded into the parent feed (a background subagent interleaves
//!   with the parent's own stream — folding them in split contiguous text
//!   around phantom tool calls).
//! - Steering: queued [`SteerMessage`]s are written to stdin as user lines at
//!   any time; the CLI folds them into the running turn at its own step
//!   boundary.
//! - Interrupt: cancelling [`RunControls::interrupt`] sends the protocol-level
//!   interrupt control request, then escalates to SIGTERM and SIGKILL.

pub mod catalog;
mod normalize;
pub mod permissions;
#[cfg(test)]
mod tests_mcp;
mod wire;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use komet_proto::{
    AgentEvent, CommandScope, CommandSource, DoneStatus, HarnessId, Model, ReasoningLevel,
    RunRequest, SlashCommand, SteeringMode, UserInputAnswer, UserInputQuestion,
};

use crate::{
    Harness, HarnessError, RunControls, Signal, errors::typed_error, send_signal, shutdown_child,
};
use catalog::{apply_ultrathink, to_effort};
use normalize::Normalizer;
use wire::{ControlRequestFrame, Frame, allow_response, control_response_line};

/// Locate the device's installed Claude Code CLI: `CLAUDE_CODE_EXECUTABLE`,
/// then our own PATH, then the login-shell PATH snapshot (the user's shell
/// init shapes PATH in ways a GUI/service launch never sees — see
/// [`crate::shell_env`]), then known install locations as a last resort.
/// Resolved per call — cheap after the snapshot is cached.
fn resolve_claude_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLAUDE_CODE_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
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
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".claude").join("local").join("claude"));
        candidates.push(home.join(".local").join("bin").join("claude"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
    candidates.push(PathBuf::from("/usr/local/bin/claude"));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}

fn option_is_on(options: &serde_json::Map<String, Value>, key: &str) -> bool {
    match options.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "on" || s == "true",
        _ => false,
    }
}

/// The Claude Code harness. Construct with [`ClaudeHarness::new`]; tests point
/// it at a fake CLI with [`ClaudeHarness::with_executable`].
pub struct ClaudeHarness {
    executable: Option<PathBuf>,
    /// Grace between the interrupt control request and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Command discovery cache keyed by probe cwd; the stored buster
    /// invalidates it when the CLI's command/skill dirs change.
    commands: tokio::sync::Mutex<std::collections::HashMap<String, (u128, Vec<SlashCommand>)>>,
    /// Model discovery cache: live probe cached on success.
    models: tokio::sync::Mutex<Option<(std::time::Instant, Vec<Model>)>>,
    /// Single-flight for model probes: held across cache-check + probe +
    /// store, so concurrent `models()` calls share one CLI spawn — waiters
    /// then read the fresh cache instead of spawning their own.
    models_probe: tokio::sync::Mutex<()>,
}

impl Default for ClaudeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            commands: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            models: tokio::sync::Mutex::new(None),
            models_probe: tokio::sync::Mutex::new(()),
        }
    }
}

impl ClaudeHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
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
        resolve_claude_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "claude (searched PATH, the login shell's PATH, ~/.claude/local, \
                 ~/.local/bin, /opt/homebrew/bin, /usr/local/bin, and \
                 fnm/nvm/volta/pnpm/bun install dirs; set CLAUDE_CODE_EXECUTABLE \
                 to override)"
                    .into(),
            )
        })
    }

    pub(crate) fn build_command(&self, exe: &PathBuf, request: &RunRequest) -> Command {
        let mut cmd = Command::new(exe);
        crate::compose_child_path(&mut cmd, exe);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Required by the CLI alongside `-p --output-format stream-json`.
            "--verbose",
            "--include-partial-messages",
            // Newer Claude models emit no readable thinking text unless a
            // summary is asked for (raw reasoning stays provider-private).
            "--thinking-display",
            "summarized",
            // Route permission prompts to the stdio control channel so
            // `can_use_tool` (and AskUserQuestion in particular) reaches us.
            // Undocumented flag; validated live against 2.1.228.
            "--permission-prompt-tool",
            "stdio",
        ]);
        // The 1M context window is selected via a model-id suffix
        // (`sonnet[1m]`), exactly how the CLI itself does it; fast mode and
        // always-on thinking are settings overrides.
        if let Some(model) = &request.model {
            let one_m = request
                .model_options
                .get("contextWindow")
                .and_then(Value::as_str)
                == Some("1m");
            cmd.arg("--model");
            // Ids discovered from the CLI's own `list_models` can already carry
            // the suffix (`opus[1m]`); appending again would send a doubled one.
            cmd.arg(if one_m && !model.contains("[1m]") {
                format!("{model}[1m]")
            } else {
                model.clone()
            });
        }
        if let Some(effort) = to_effort(request.reasoning, request.model.as_deref()) {
            cmd.args(["--effort", effort]);
        }
        // Permission mode. Legacy runs (no `sandbox_options`) keep yolo:
        // auto_approve bypasses everything, parity with the codex adapter.
        // An explicit `SandboxOptions` table restricts: a `claude` entry WINS
        // via its own `allow_unsandboxed_commands`; otherwise (entry absent or
        // non-restricting) we fall back to the request-level sandbox level,
        // so e.g. `sandbox = ReadOnly` with only a `codex` table stays safe.
        let claude_opts = request
            .sandbox_options
            .as_ref()
            .and_then(|o| o.claude.as_ref())
            // Paseo `enabled: false` turns the whole Claude sandbox OFF — the
            // table translation below is skipped entirely (no permission
            // table, no settings.sandbox). Restriction fields become no-ops.
            .filter(|c| c.enabled != Some(false));
        let unrestricted = match request.sandbox_options.as_ref() {
            None => true,
            Some(o) => match &o.claude {
                Some(c) => c.allow_unsandboxed_commands,
                None => request.sandbox == komet_proto::SandboxLevel::DangerFullAccess,
            },
        };
        if request.auto_approve && unrestricted {
            cmd.args([
                "--permission-mode",
                "bypassPermissions",
                "--dangerously-skip-permissions",
            ]);
        } else {
            cmd.args(["--permission-mode", "default"]);
        }
        if let Some(resume) = &request.resume {
            cmd.arg(format!("--resume={resume}"));
        }
        let mut settings = serde_json::Map::new();
        if option_is_on(&request.model_options, "fastMode") {
            settings.insert("fastMode".into(), Value::Bool(true));
        }
        if option_is_on(&request.model_options, "thinking") {
            settings.insert("alwaysThinkingEnabled".into(), Value::Bool(true));
        }
        if request.reasoning == Some(ReasoningLevel::Ultracode) {
            settings.insert("ultracode".into(), Value::Bool(true));
        }
        // Restricted-sandbox translation: the table maps onto Claude's
        // `settings.permissions` surface. `settings_permissions` passthrough
        // merges LAST so raw Claude-side keys can override anything here.
        if let Some(c) = claude_opts {
            let (perms, sandbox) = permissions::build_claude_settings_maps(c);
            if !sandbox.is_empty() {
                settings.insert("sandbox".into(), Value::Object(sandbox));
            }
            if !perms.is_empty() {
                settings.insert("permissions".into(), Value::Object(perms));
            }
        }
        if !settings.is_empty() {
            cmd.arg("--settings");
            cmd.arg(Value::Object(settings).to_string());
        }
        // MCP injection: merge internal `mcp` + external `mcp_external` into
        // `mcpServers` JSON for `--mcp-config`. Deduplicate by `id`, suppress
        // duplicate if `url` identical, and respect `always_load` (do not
        // overwrite an existing `id` without it). Secrets (headers/env) ride
        // only at launch via `mcp_external`; logs mask values.
        {
            let mut servers = serde_json::Map::new();
            if let Some(mcp) = &request.mcp {
                servers.insert(
                    mcp.server_name.clone(),
                    serde_json::json!({
                        "type": "http",
                        "url": mcp.url,
                        "headers": {
                            "Authorization": format!("Bearer {}", mcp.auth_token)
                        }
                    }),
                );
            }
            for ext in &request.mcp_external {
                // Claude's CLI takes resolved MCP JSON as an argv value.
                if ext.config.id == "composio" {
                    continue;
                }
                let id = ext.config.id.clone();
                if servers.contains_key(&id) {
                    let existing_url = servers
                        .get(&id)
                        .and_then(|v| v.get("url"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let new_url = ext.config.url.clone();
                    if existing_url == new_url {
                        tracing::debug!(server_id=%id, "claude mcp dedup by id/url (masked)");
                        continue;
                    }
                    if !ext.config.always_load {
                        tracing::debug!(server_id=%id, "claude mcp skip overwrite without always_load (masked)");
                        continue;
                    }
                }
                if let Some(new_url) = ext.config.url.as_deref() {
                    let dup = servers
                        .values()
                        .any(|v| v.get("url").and_then(Value::as_str) == Some(new_url));
                    if dup {
                        tracing::debug!(server_id=%id, "claude mcp dedup by url (masked)");
                        continue;
                    }
                }
                let entry = match ext.config.transport {
                    komet_proto::McpTransport::Stdio => {
                        let mut obj = serde_json::Map::new();
                        if let Some(cmd_str) = ext.config.command.clone() {
                            obj.insert("command".into(), Value::String(cmd_str));
                        }
                        obj.insert(
                            "args".into(),
                            Value::Array(
                                ext.config.args.iter().cloned().map(Value::String).collect(),
                            ),
                        );
                        let env_src = if !ext.resolved_env.is_empty() {
                            &ext.resolved_env
                        } else {
                            &ext.config.env
                        };
                        if !env_src.is_empty() {
                            obj.insert(
                                "env".into(),
                                Value::Object(
                                    env_src
                                        .iter()
                                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                        .collect(),
                                ),
                            );
                        }
                        Value::Object(obj)
                    }
                    komet_proto::McpTransport::Http => {
                        let mut obj = serde_json::Map::new();
                        obj.insert("type".into(), Value::String("http".into()));
                        if let Some(url) = ext.config.url.clone() {
                            obj.insert("url".into(), Value::String(url));
                        }
                        let headers = if !ext.resolved_headers.is_empty() {
                            &ext.resolved_headers
                        } else {
                            &ext.config.headers
                        };
                        if !headers.is_empty() {
                            obj.insert(
                                "headers".into(),
                                Value::Object(
                                    headers
                                        .iter()
                                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                        .collect(),
                                ),
                            );
                        }
                        if !ext.resolved_env.is_empty() {
                            obj.insert(
                                "env".into(),
                                Value::Object(
                                    ext.resolved_env
                                        .iter()
                                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                        .collect(),
                                ),
                            );
                        }
                        Value::Object(obj)
                    }
                    komet_proto::McpTransport::Sse => {
                        let mut obj = serde_json::Map::new();
                        obj.insert("type".into(), Value::String("sse".into()));
                        if let Some(url) = ext.config.url.clone() {
                            obj.insert("url".into(), Value::String(url));
                        }
                        let headers = if !ext.resolved_headers.is_empty() {
                            &ext.resolved_headers
                        } else {
                            &ext.config.headers
                        };
                        if !headers.is_empty() {
                            obj.insert(
                                "headers".into(),
                                Value::Object(
                                    headers
                                        .iter()
                                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                        .collect(),
                                ),
                            );
                        }
                        if !ext.resolved_env.is_empty() {
                            obj.insert(
                                "env".into(),
                                Value::Object(
                                    ext.resolved_env
                                        .iter()
                                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                                        .collect(),
                                ),
                            );
                        }
                        Value::Object(obj)
                    }
                };
                servers.insert(id, entry);
            }
            if !servers.is_empty() {
                let config = serde_json::json!({"mcpServers": servers}).to_string();
                cmd.args(["--mcp-config", &config]);
                tracing::debug!(
                    server_count = servers.len(),
                    "claude mcpServers injected (values masked)"
                );
            }
        }
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Short-lived discovery probe: spawn the CLI in stream-json mode, send
    /// the `initialize` and `get_context_usage` control requests, and combine
    /// static built-in commands with dynamically discovered skills.
    async fn discover_commands(&self, cwd: &Path) -> Result<Vec<SlashCommand>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.current_dir(cwd);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Mandatory with --print + stream-json output; without it the
            // CLI exits immediately with a usage error.
            "--verbose",
        ]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("claude child has no stdio".into()));
        };
        const INIT_PROBE_ID: &str = "komet-command-probe";
        const CONTEXT_PROBE_ID: &str = "komet-context-probe";
        let discovery = async {
            let init_req = serde_json::json!({
                "type": "control_request",
                "request_id": INIT_PROBE_ID,
                "request": { "subtype": "initialize" },
            });
            let context_req = wire::context_usage_request_line(CONTEXT_PROBE_ID);
            stdin
                .write_all(format!("{init_req}\n{context_req}\n").as_bytes())
                .await
                .map_err(HarnessError::Io)?;
            stdin.flush().await.map_err(HarnessError::Io)?;

            let mut init_commands: Vec<SlashCommand> = Vec::new();
            let mut skill_commands: Vec<SlashCommand> = Vec::new();
            let mut got_init = false;
            let mut got_context = false;

            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await.map_err(HarnessError::Io)? {
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("type").and_then(Value::as_str) != Some("control_response") {
                    continue;
                }
                let response = frame.get("response").cloned().unwrap_or(Value::Null);
                let req_id = response.get("request_id").and_then(Value::as_str);
                if req_id == Some(INIT_PROBE_ID) {
                    got_init = true;
                    if response.get("subtype").and_then(Value::as_str) != Some("error") {
                        init_commands = parse_initialize_commands(&response);
                    }
                } else if req_id == Some(CONTEXT_PROBE_ID) {
                    got_context = true;
                    if response.get("subtype").and_then(Value::as_str) != Some("error") {
                        skill_commands = wire::parse_skills_frontmatter(&response);
                    }
                }
                if got_init && got_context {
                    break;
                }
            }

            // The live wire is authoritative.  The old implementation
            // prepended a curated TUI catalog, which made commands appear in
            // Komet even when this CLI version did not offer them.  Merge
            // only commands actually returned by the control channel and
            // the project/user skill probe.
            let mut commands = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for command in skill_commands.into_iter().chain(init_commands) {
                if seen.insert(command.name.clone()) {
                    commands.push(command);
                }
            }
            mark_project_commands(&mut commands, cwd);

            Ok(commands)
        };
        let result = tokio::time::timeout(Duration::from_secs(10), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol(
                "Claude command discovery timed out".into(),
            )),
        }
    }

    /// Live model discovery via the Claude control channel.
    ///
    /// The subtype has moved across CLI generations: 2.1.x answers
    /// `list_models` (each entry carries `value`/`resolvedModel`/`displayName`
    /// and the CLI's own `supportedEffortLevels`) and rejects the SDK-style
    /// `supported_models` the TS harness used, while older builds answer the
    /// latter. Both are probed, newest first, so either generation works.
    /// A CLI that answers neither produces that CLI's own error message
    /// instead of a bare "no usable models", which is what a user can act on.
    pub(crate) async fn discover_live_models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("claude child has no stdio".into()));
        };

        const MODELS_PROBE_ID: &str = "komet-models-probe";
        // Probed in order; the next one is only tried when the CLI explicitly
        // rejects the previous as an unsupported subtype.
        const MODEL_PROBE_SUBTYPES: [&str; 2] = ["list_models", "supported_models"];
        let discovery = async {
            let mut lines = BufReader::new(stdout).lines();
            let mut rejected: Vec<String> = Vec::new();
            for (attempt, subtype) in MODEL_PROBE_SUBTYPES.iter().enumerate() {
                let request_id = format!("{MODELS_PROBE_ID}-{attempt}");
                let req = serde_json::json!({
                    "type": "control_request",
                    "request_id": request_id,
                    "request": { "subtype": subtype },
                });
                stdin
                    .write_all(format!("{req}\n").as_bytes())
                    .await
                    .map_err(HarnessError::Io)?;
                stdin.flush().await.map_err(HarnessError::Io)?;

                let mut discovered_models: Vec<Model> = Vec::new();
                let mut cli_error: Option<String> = None;
                let mut answered = false;
                while let Some(line) = lines.next_line().await.map_err(HarnessError::Io)? {
                    let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if frame.get("type").and_then(Value::as_str) != Some("control_response") {
                        continue;
                    }
                    let response = frame.get("response").cloned().unwrap_or(Value::Null);
                    if response.get("request_id").and_then(Value::as_str)
                        != Some(request_id.as_str())
                    {
                        continue;
                    }
                    answered = true;
                    if response.get("subtype").and_then(Value::as_str) == Some("error") {
                        cli_error = Some(
                            response
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown control error")
                                .to_owned(),
                        );
                    } else {
                        let resp_body = response.get("response").unwrap_or(&response);
                        discovered_models = catalog::parse_discovered_models(resp_body);
                    }
                    break;
                }

                if !discovered_models.is_empty() {
                    return Ok::<Vec<Model>, HarnessError>(discovered_models);
                }
                let Some(cli_error) = cli_error else {
                    if answered {
                        rejected.push(format!("{subtype}: answered without a model list"));
                    }
                    break;
                };
                let unsupported = cli_error.to_ascii_lowercase().contains("unsupported");
                rejected.push(format!("{subtype}: {cli_error}"));
                if !unsupported {
                    break;
                }
            }

            Err(HarnessError::Protocol(format!(
                "claude model discovery failed ({})",
                rejected.join("; ")
            )))
        };

        // A cold `claude --print` boot (auth check, MCP/config load) routinely
        // runs past 10s on a busy machine — too tight a bound turned a healthy
        // CLI into "harness protocol error: claude model discovery timed out".
        // 45s absorbs a cold start; the engine's cache/fallback layer keeps the
        // picker populated while it runs.
        let result = tokio::time::timeout(Duration::from_secs(45), discovery).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol(
                "claude model discovery timed out".into(),
            )),
        }
    }

    /// Query structured context usage (tokens, categories, loaded files, MCP tools, skills)
    /// from the CLI via `get_context_usage` control request.
    pub async fn get_context_usage(&self) -> Result<komet_proto::ContextUsage, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            shutdown_child(&mut child, self.kill_grace).await;
            return Err(HarnessError::Protocol("claude child has no stdio".into()));
        };
        const REQ_ID: &str = "komet-context-usage";
        let probe = async {
            let request = wire::context_usage_request_line(REQ_ID);
            stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .map_err(HarnessError::Io)?;
            stdin.flush().await.map_err(HarnessError::Io)?;
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await.map_err(HarnessError::Io)? {
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("type").and_then(Value::as_str) != Some("control_response") {
                    continue;
                }
                let response = frame.get("response").cloned().unwrap_or(Value::Null);
                if response.get("request_id").and_then(Value::as_str) != Some(REQ_ID) {
                    continue;
                }
                if response.get("subtype").and_then(Value::as_str) == Some("error") {
                    let msg = response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("get_context_usage control request failed");
                    return Err(HarnessError::Protocol(msg.into()));
                }
                return Ok(wire::parse_context_usage_response(&response));
            }
            Err(HarnessError::Protocol(
                "claude exited before answering get_context_usage".into(),
            ))
        };
        let result = tokio::time::timeout(Duration::from_secs(10), probe).await;
        shutdown_child(&mut child, self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("get_context_usage timed out".into())),
        }
    }
}

/// Mark commands backed by a project-local `.claude/commands/<name>.md` or
/// `.claude/skills/<name>` as project-scoped skill entries. The initialize
/// wire does not say where a command came from; the file's presence does.
fn mark_project_commands(commands: &mut [SlashCommand], cwd: &Path) {
    for command in commands {
        let from_project = [
            cwd.join(".claude/commands")
                .join(format!("{}.md", command.name)),
            cwd.join(".claude/commands").join(&command.name),
            cwd.join(".claude/skills").join(&command.name),
        ]
        .iter()
        .any(|path| path.exists());
        if from_project {
            command.source = CommandSource::Skill;
            command.scope = CommandScope::Project;
        }
    }
}

/// `commands` out of an `initialize` control_response payload
/// (`response.response.commands`: name / description / argumentHint).
fn parse_initialize_commands(response: &Value) -> Vec<SlashCommand> {
    response
        .get("response")
        .and_then(|r| r.get("commands"))
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            Some(SlashCommand {
                name: name.to_owned(),
                description: c
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_hint: c
                    .get("argumentHint")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(str::to_owned),
                ..SlashCommand::default()
            })
        })
        .collect()
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn display_name(&self) -> &str {
        "Claude Code"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    }
    fn installed(&self) -> bool {
        self.executable.is_some() || resolve_claude_executable().is_some()
    }
    /// Done is the CLI's own terminal frame, for wake turns too.
    fn deterministic_turn_end(&self) -> bool {
        true
    }

    /// Live model discovery via control-channel probe. Errors propagate —
    /// no static fallback, so the picker only shows models the CLI offers.
    /// A failed probe is UNCACHED so the next picker open retries.
    /// Concurrent calls share one probe (single-flight via `models_probe`).
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        let _probe = self.models_probe.lock().await;
        {
            let guard = self.models.lock().await;
            if let Some((at, models)) = guard.as_ref() {
                // In-session memo only: long enough that a switch back to this
                // harness never re-spawns the probe, short enough that a
                // `Rescan` past it still sees a fresh CLI.
                if at.elapsed() < std::time::Duration::from_secs(60) {
                    return Ok(models.clone());
                }
            }
        }
        let models = self.discover_live_models().await.inspect_err(|e| {
            tracing::warn!(
                target: "komet_harness::claude",
                "Claude live model discovery failed: {e}"
            );
        })?;
        if models.is_empty() {
            return Err(HarnessError::Protocol(
                "claude supported_models returned no usable models".into(),
            ));
        }
        tracing::info!(
            target: "komet_harness::claude",
            count = models.len(),
            "discovered live Claude models from control channel"
        );
        *self.models.lock().await = Some((std::time::Instant::now(), models.clone()));
        Ok(models)
    }

    async fn refresh_models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        let _probe = self.models_probe.lock().await;
        let models = self.discover_live_models().await?;
        if models.is_empty() {
            return Err(HarnessError::Protocol(
                "claude supported_models returned no usable models".into(),
            ));
        }
        *self.models.lock().await = Some((std::time::Instant::now(), models.clone()));
        Ok(models)
    }

    /// Curated catalog, shown only until the first live probe succeeds (see
    /// [`Harness::fallback_models`]). Not served by `models()` — that stays
    /// live-only and error-honest.
    fn fallback_models(&self) -> Vec<Model> {
        catalog::static_models()
    }

    /// Slash commands from the CLI's `initialize` control-request handshake —
    /// the same channel the Claude Agent SDK's `query()` opens. The response
    /// carries every command with description + argument hint and involves no
    /// model turn (verified live, 2.1.228: the control_response is the first
    /// stdout line, well before any API traffic). Cached on success.
    async fn commands(&self, cwd: Option<&Path>) -> Result<Vec<SlashCommand>, HarnessError> {
        let cwd = crate::discovery_cwd(cwd);
        let key = cwd.to_string_lossy().into_owned();
        let buster = crate::commands_buster(&cwd, HarnessId::ClaudeCode);
        if let Some((stored, commands)) = self.commands.lock().await.get(&key)
            && *stored == buster
        {
            return Ok(commands.clone());
        }
        let commands = self.discover_commands(&cwd).await?;
        self.commands
            .lock()
            .await
            .insert(key, (buster, commands.clone()));
        Ok(commands)
    }

    async fn context_usage(&self) -> Result<komet_proto::ContextUsage, HarnessError> {
        self.get_context_usage().await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = self.build_command(&exe, &request);
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
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("claude child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "komet_harness::claude", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<StdinMsg>();
        tokio::spawn(stdin_writer(stdin, stdin_rx));

        // The initial prompt as the first stdin user line (streaming-input
        // mode). Ultrathink rides every user message — steers included.
        // Staged image attachments are inlined as base64 image content blocks
        // ahead of the text (verified against the real CLI); their path refs
        // also ride the prompt text, so a skipped/unreadable file degrades to
        // the old-app behavior (the agent opens the path with its Read tool).
        let images = load_image_blocks(&request.attachments).await;
        let first = wire::user_message_line_with_images(
            &apply_ultrathink(request.reasoning, &request.prompt),
            &images,
        );
        let _ = stdin_tx.send(StdinMsg::Line(first));

        let auto_approve = request.auto_approve;
        let unrestricted = match request.sandbox_options.as_ref() {
            None => true,
            Some(o) => match &o.claude {
                Some(c) => c.allow_unsandboxed_commands,
                None => request.sandbox == komet_proto::SandboxLevel::DangerFullAccess,
            },
        };
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            stdout_lines: BufReader::new(stdout).lines(),
            stdin_tx,
            event_tx,
            controls,
            reasoning: request.reasoning,
            auto_approve,
            unrestricted,
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

enum StdinMsg {
    Line(String),
    /// Close stdin (end of steering input): the CLI finishes the current turn
    /// and exits, which ends the run stream at stdout EOF.
    Close,
}

const LIVE_CONTEXT_REQ_ID: &str = "komet-live-context";

fn live_context_window(frame: &Value) -> Option<AgentEvent> {
    let response = frame.get("response")?;
    if response.get("request_id").and_then(Value::as_str) != Some(LIVE_CONTEXT_REQ_ID) {
        return None;
    }
    if response.get("subtype").and_then(Value::as_str) == Some("error") {
        return None;
    }
    let usage = wire::parse_context_usage_response(response);
    Some(AgentEvent::ContextWindow {
        stats: komet_proto::ContextUsageStats::window(
            usage.total_tokens,
            usage.max_tokens,
            komet_proto::ContextUsageSource::Native,
            usage.total_tokens,
            0,
            0,
            0,
        ),
    })
}

/// Anthropic's API caps inline images at 5MB of raw bytes; larger files stay
/// path refs only.
const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Media type for an inline image block — extension first, magic bytes as the
/// fallback (pasted screenshots may carry odd names). Only the API-supported
/// inline types map; anything else (svg/bmp/tiff/…) returns `None`.
fn image_media_type(path: &std::path::Path, bytes: &[u8]) -> Option<&'static str> {
    let by_ext = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    by_ext.or(match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    })
}

/// Load `RunRequest::attachments` into inline image blocks, best-effort: an
/// unreadable, oversized, or unsupported file is skipped — its path ref still
/// rides the prompt text — never fatal to the run.
async fn load_image_blocks(paths: &[String]) -> Vec<wire::ImageBlock> {
    use base64::Engine as _;
    let mut blocks = Vec::new();
    for path in paths {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(target: "komet_harness::claude", %path, error = %err, "attachment unreadable; path ref only");
                continue;
            }
        };
        if bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            tracing::debug!(target: "komet_harness::claude", %path, "attachment over inline cap; path ref only");
            continue;
        }
        let Some(media_type) = image_media_type(std::path::Path::new(path), &bytes) else {
            tracing::debug!(target: "komet_harness::claude", %path, "attachment not an inline-supported image; path ref only");
            continue;
        };
        blocks.push(wire::ImageBlock {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    blocks
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged.
async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<StdinMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            StdinMsg::Line(line) => {
                let write = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if let Err(e) = write.await {
                    tracing::debug!(target: "komet_harness::claude", "stdin write failed (tolerated): {e}");
                    return;
                }
            }
            StdinMsg::Close => {
                let _ = stdin.shutdown().await;
                return;
            }
        }
    }
}

struct Session {
    child: Child,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin_tx: mpsc::UnboundedSender<StdinMsg>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    reasoning: Option<ReasoningLevel>,
    auto_approve: bool,
    unrestricted: bool,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
}

/// The per-run event loop: one task multiplexing stdout frames, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        mut stdout_lines,
        stdin_tx,
        event_tx,
        controls,
        reasoning,
        auto_approve,
        unrestricted,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        request_permission,
        mut steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);
    let request_permission = Arc::new(request_permission);

    let mut norm = Normalizer::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut any_done = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame = match wire::parse_frame(line) {
                        Ok(frame) => frame,
                        Err(e) => {
                            tracing::debug!(target: "komet_harness::claude", "unparseable frame (skipped): {e}");
                            continue;
                        }
                    };
                    if let Frame::ControlRequest(req) = frame {
                        handle_control_request(
                            req,
                            &request_input,
                            &request_permission,
                            auto_approve,
                            unrestricted,
                            &stdin_tx,
                        );
                        continue;
                    }
                    if let Frame::ControlResponse(value) = frame {
                        if let Some(ev) = live_context_window(&value)
                            && event_tx.send(Ok(ev)).await.is_err()
                        {
                            break 'main;
                        }
                        continue;
                    }
                    let probe_context = matches!(&frame, Frame::Result(_));
                    for ev in norm.normalize(frame, interrupted) {
                        let is_done = matches!(ev, AgentEvent::Done { .. });
                        if event_tx.send(Ok(ev)).await.is_err() {
                            break 'main; // consumer gone — reap below
                        }
                        if is_done {
                            any_done = true;
                            if interrupted {
                                done_after_interrupt = true;
                                break 'main;
                            }
                        }
                    }
                    if probe_context {
                        let _ = stdin_tx.send(StdinMsg::Line(wire::context_usage_request_line(
                            LIVE_CONTEXT_REQ_ID,
                        )));
                    }
                }
                Ok(None) => break 'main, // stdout EOF: the CLI exited
                Err(e) => {
                    let _ = event_tx.send(Err(HarnessError::Io(e))).await;
                    break 'main;
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let line = wire::user_message_line(&apply_ultrathink(reasoning, &msg.prompt));
                    let _ = stdin_tx.send(StdinMsg::Line(line));
                    // The CLI consumes the queued line at its own step
                    // boundary; rotate the assistant message id so post-steer
                    // output folds into a fresh message.
                    let (prev, next) = norm.rotate_for_steer();
                    let ev = AgentEvent::Steered {
                        assistant_message_id: Some(prev),
                        next_assistant_message_id: Some(next),
                    };
                    if event_tx.send(Ok(ev)).await.is_err() {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed: end the input so the run can finish
                    // after the current turn.
                    steering_open = false;
                    let _ = stdin_tx.send(StdinMsg::Close);
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                let _ = stdin_tx.send(StdinMsg::Line(wire::interrupt_request_line("int_1")));
                // Escalate if the CLI doesn't wind down within the grace
                // periods: SIGTERM (kills bash trees, runs SessionEnd hooks),
                // then SIGKILL. Aborted once the child is reaped.
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

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: norm.session_id.clone(),
                    reason: None,
                }))
                .await;
        } else if !interrupted && !any_done {
            let status = child.try_wait().ok().flatten();
            let crash = crate::crash_message("claude", status, &stderr_tail);
            let _ = event_tx
                .send(Ok(typed_error(
                    crash.clone(),
                    None,
                    crate::errors::tail_detail(stderr_tail.snapshot()),
                )))
                .await;
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crash),
                    session_id: norm.session_id.clone(),
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

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// Serve one `can_use_tool` control request. Unattended/auto_approve runs
/// auto-allow immediately; `AskUserQuestion` is intercepted and routed
/// to the engine's input bridge; other tool requests route to the permission
/// bridge when not auto-approved.
fn handle_control_request(
    req: ControlRequestFrame,
    request_input: &Arc<RequestInputFn>,
    request_permission: &Arc<crate::RequestPermissionFn>,
    auto_approve: bool,
    unrestricted: bool,
    stdin_tx: &mpsc::UnboundedSender<StdinMsg>,
) {
    if req.request.subtype != "can_use_tool" {
        tracing::debug!(
            target: "komet_harness::claude",
            "unhandled control_request subtype: {}", req.request.subtype
        );
        return;
    }
    if req.request.tool_name == "AskUserQuestion" {
        let request_input = Arc::clone(request_input);
        let stdin_tx = stdin_tx.clone();
        tokio::spawn(async move {
            let request_id = req.request_id;
            let input = req.request.input;
            let questions = parse_questions(&input);
            let answers = (request_input)(questions.clone()).await.unwrap_or_default();
            let updated = updated_input_with_answers(&input, &questions, &answers);
            let line = control_response_line(&request_id, allow_response(updated));
            let _ = stdin_tx.send(StdinMsg::Line(line));
        });
        return;
    }

    if auto_approve && unrestricted {
        let line = control_response_line(&req.request_id, allow_response(req.request.input));
        let _ = stdin_tx.send(StdinMsg::Line(line));
        return;
    }

    let request_permission = Arc::clone(request_permission);
    let stdin_tx = stdin_tx.clone();
    tokio::spawn(async move {
        let request_id = req.request_id;
        let input = req.request.input;
        let (kind, summary, choices) =
            permissions::parse_tool_permission(&req.request.tool_name, &input);
        let decision = (request_permission)(kind, summary, choices)
            .await
            .unwrap_or(komet_proto::PermissionChoice::Deny);
        let resp = match decision {
            komet_proto::PermissionChoice::Allow
            | komet_proto::PermissionChoice::AllowAlways { .. } => allow_response(input),
            komet_proto::PermissionChoice::Deny => {
                wire::deny_response(Some("Permission denied by user"))
            }
        };
        let line = control_response_line(&request_id, resp);
        let _ = stdin_tx.send(StdinMsg::Line(line));
    });
}

/// Parse Claude's `AskUserQuestion` tool input into [`UserInputQuestion`]s
/// (tolerant of `header`/`title`, `question`/`prompt`, string or object
/// options — option descriptions are dropped, the wire type carries labels).
fn parse_questions(input: &Value) -> Vec<UserInputQuestion> {
    let raw = input.get("questions").and_then(Value::as_array);
    raw.map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|q| {
            let field =
                |keys: [&str; 2]| keys.iter().find_map(|k| q.get(*k).and_then(Value::as_str));
            UserInputQuestion {
                id: uuid::Uuid::new_v4().to_string(),
                header: field(["header", "title"]).unwrap_or("Question").into(),
                question: field(["question", "prompt"]).unwrap_or("").into(),
                multi_select: ["multiSelect", "multi_select"]
                    .iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_bool))
                    .unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|op| match op {
                        Value::String(s) => s.clone(),
                        other => other
                            .get("label")
                            .or_else(|| other.get("value"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Merge the user's answers back into the tool input, keyed by question text
/// (single-select ⇒ a string, multi-select ⇒ an array), as the tool expects.
fn updated_input_with_answers(
    input: &Value,
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> Value {
    let mut updated = match input {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut by_question = serde_json::Map::new();
    for q in questions {
        let labels: Vec<String> = answers
            .iter()
            .find(|a| a.question_id == q.id)
            .map(|a| a.labels.clone())
            .unwrap_or_default();
        let value = if q.multi_select {
            Value::Array(labels.into_iter().map(Value::String).collect())
        } else {
            Value::String(labels.into_iter().next().unwrap_or_default())
        };
        by_question.insert(q.question.clone(), value);
    }
    updated.insert("answers".into(), Value::Object(by_question));
    Value::Object(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_questions_tolerantly() {
        let input = json!({
            "questions": [
                {
                    "header": "Choice",
                    "question": "Pick one",
                    "options": ["A", {"label": "B", "description": "second"}],
                    "multiSelect": false
                },
                { "title": "Alt", "prompt": "Pick many", "multi_select": true }
            ]
        });
        let qs = parse_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header, "Choice");
        assert_eq!(qs[0].options, vec!["A".to_string(), "B".to_string()]);
        assert!(!qs[0].multi_select);
        assert_eq!(qs[1].header, "Alt");
        assert_eq!(qs[1].question, "Pick many");
        assert!(qs[1].multi_select);
    }

    #[test]
    fn answers_key_by_question_text() {
        let input =
            json!({"questions": [{"header": "H", "question": "Pick one", "options": ["A", "B"]}]});
        let qs = parse_questions(&input);
        let answers = vec![UserInputAnswer {
            question_id: qs[0].id.clone(),
            labels: vec!["B".into()],
        }];
        let updated = updated_input_with_answers(&input, &qs, &answers);
        assert_eq!(updated["answers"]["Pick one"], json!("B"));
        // Original input is preserved alongside the answers.
        assert!(updated["questions"].is_array());
    }

    fn base_request() -> RunRequest {
        RunRequest {
            prompt: "hi".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: String::new(),
            sandbox: komet_proto::SandboxLevel::DangerFullAccess,
            sandbox_options: None,
            auto_approve: true,
            attachments: Vec::new(),
            permission_timeout_ms: None,
            worktree: None,
            resume: None,
            mcp: None,
            mcp_external: Vec::new(),
        }
    }

    fn argv(cmd: &Command) -> Vec<String> {
        let std_cmd = cmd.as_std();
        std::iter::once(std_cmd.get_program().to_string_lossy().to_string())
            .chain(std_cmd.get_args().map(|a| a.to_string_lossy().to_string()))
            .collect()
    }

    fn settings_json(args: &[String]) -> serde_json::Map<String, Value> {
        let idx = args
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings present");
        let parsed: Value = serde_json::from_str(&args[idx + 1]).expect("valid settings JSON");
        parsed.as_object().cloned().expect("settings object")
    }

    #[test]
    fn old_client_yolo_unchanged_without_sandbox_options() {
        let h = ClaudeHarness::new();
        let cmd = h.build_command(&PathBuf::from("claude"), &base_request());
        let args = argv(&cmd);
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
    }

    #[test]
    fn restricted_claude_sandbox_blocks_skip_permissions_and_sets_permissions() {
        let mut req = base_request();
        req.sandbox_options = Some(komet_proto::SandboxOptions {
            claude: Some(komet_proto::ClaudeSandbox {
                filesystem: komet_proto::FilesystemSandbox {
                    allow: vec!["/work/src".into()],
                    deny: vec!["/etc".into()],
                    ..Default::default()
                },
                allow_unsandboxed_commands: false,
                excluded_commands: vec!["rm".into()],
                ..Default::default()
            }),
            ..Default::default()
        });
        // Even with auto_approve, a restricted sandbox must NOT bypass.
        let h = ClaudeHarness::new();
        let args = argv(&h.build_command(&PathBuf::from("claude"), &req));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "default"])
        );
        let settings = settings_json(&args);
        let perms = settings["permissions"]
            .as_object()
            .expect("permissions object");
        assert_eq!(perms["defaultMode"], json!("default"));
        let deny = perms["deny"].as_array().expect("deny list");
        assert!(deny.contains(&json!("Bash(rm:*)")));
        assert!(deny.contains(&json!("Edit(/etc)")));
        assert_eq!(
            perms["additionalDirectories"].as_array().unwrap(),
            &vec![json!("/work/src")]
        );
    }

    #[test]
    fn allow_unsandboxed_commands_keeps_yolo() {
        let mut req = base_request();
        req.sandbox_options = Some(komet_proto::SandboxOptions {
            claude: Some(komet_proto::ClaudeSandbox {
                allow_unsandboxed_commands: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let h = ClaudeHarness::new();
        let args = argv(&h.build_command(&PathBuf::from("claude"), &req));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn non_claude_sandbox_table_leaves_yolo_alone() {
        let mut req = base_request();
        req.sandbox_options = Some(komet_proto::SandboxOptions {
            opencode: Some(komet_proto::OpenCodePerms::default()),
            ..Default::default()
        });
        let h = ClaudeHarness::new();
        let args = argv(&h.build_command(&PathBuf::from("claude"), &req));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn codex_only_table_with_read_only_sandbox_restricts_claude() {
        let mut req = base_request();
        req.sandbox = komet_proto::SandboxLevel::ReadOnly;
        req.sandbox_options = Some(komet_proto::SandboxOptions {
            codex: Some(komet_proto::CodexSandbox::default()),
            ..Default::default()
        });
        let h = ClaudeHarness::new();
        let args = argv(&h.build_command(&PathBuf::from("claude"), &req));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "default"])
        );
    }

    #[test]
    fn strict_allowlist_wire() {
        let mut req = base_request();
        req.sandbox_options = Some(komet_proto::SandboxOptions {
            claude: Some(komet_proto::ClaudeSandbox {
                network: komet_proto::NetworkSandbox {
                    allowed_hosts: vec!["crates.io".into()],
                    strict_allowlist: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        });
        let h = ClaudeHarness::new();
        let args = argv(&h.build_command(&PathBuf::from("claude"), &req));
        let settings = settings_json(&args);
        let network = settings["sandbox"]["network"]
            .as_object()
            .expect("network object");
        assert_eq!(network["strictAllowlist"], json!(true));
        assert_eq!(network["allowedDomains"], json!(["crates.io"]));
    }

    #[test]
    fn settings_permissions_passthrough_merges_into_permissions() {
        let mut req = base_request();
        req.sandbox_options = Some(komet_proto::SandboxOptions {
            claude: Some(komet_proto::ClaudeSandbox {
                allow_unsandboxed_commands: false,
                settings_permissions: json!({"allow": ["Bash(git *)"]}),
                ..Default::default()
            }),
            ..Default::default()
        });
        let h = ClaudeHarness::new();
        let args = argv(&h.build_command(&PathBuf::from("claude"), &req));
        let settings = settings_json(&args);
        assert_eq!(
            settings["permissions"]["allow"].as_array().unwrap(),
            &vec![json!("Bash(git *)")]
        );
        assert_eq!(settings["permissions"]["defaultMode"], json!("default"));
    }
}
