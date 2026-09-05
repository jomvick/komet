//! Native harness for Google Antigravity CLI (`agy`).
//!
//! Antigravity CLI exposes a documented NDJSON protocol in headless mode:
//! `agy -p … --output-format stream-json`. Each invocation emits an `init`,
//! zero or more `step_update`s, and exactly one `result`; conversation ids can
//! be resumed on a later invocation with `--conversation`.

pub mod catalog;
pub mod normalize;

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use futures::{StreamExt, stream};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use komet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand, SteeringMode,
};

use crate::{Harness, HarnessError, RunControls};

pub struct AntigravityHarness {
    executable: Option<PathBuf>,
    models: tokio::sync::OnceCell<Vec<Model>>,
}

impl AntigravityHarness {
    pub fn new() -> Self {
        Self {
            executable: None,
            models: tokio::sync::OnceCell::new(),
        }
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = &self.executable {
            return Ok(path.clone());
        }
        if let Some(path) =
            std::env::var_os("ANTIGRAVITY_CLI_EXECUTABLE").filter(|path| !path.is_empty())
        {
            return Ok(PathBuf::from(path));
        }
        let executable = if cfg!(windows) { "agy.exe" } else { "agy" };
        let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(executable))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(path) = crate::shell_env::login_shell_path() {
            paths.extend(std::env::split_paths(path).map(|dir| dir.join(executable)));
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            paths.push(home.join(".local/bin").join(executable));
            paths.push(home.join(".gemini/antigravity/bin").join(executable));
            paths.push(home.join(".config/Antigravity/bin").join(executable));
            paths.push(home.join(".local/share/antigravity/bin").join(executable));
        }
        paths.push(PathBuf::from("/usr/local/bin").join(executable));
        paths.push(PathBuf::from("/usr/bin").join(executable));
        paths.into_iter().find(|path| path.is_file()).ok_or_else(|| {
            HarnessError::NotInstalled(
                "agy (install with `curl -fsSL https://antigravity.google/cli/install.sh | bash`, then authenticate with `agy`; set ANTIGRAVITY_CLI_EXECUTABLE to override)".into(),
            )
        })
    }
}

impl Default for AntigravityHarness {
    fn default() -> Self {
        Self::new()
    }
}

fn usage_event(usage: &Value) -> AgentEvent {
    let count = |key| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
    AgentEvent::Usage {
        input_tokens: count("input_tokens"),
        cached_input_tokens: count("cache_read_tokens"),
        output_tokens: count("output_tokens"),
        reasoning_tokens: count("thinking_tokens"),
        context_limit: None,
    }
}

#[async_trait]
impl Harness for AntigravityHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Antigravity
    }
    fn display_name(&self) -> &str {
        "Antigravity CLI"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        catalog::FULL_LADDER
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let executable = self.resolve_executable()?;
        self.models
            .get_or_try_init(|| async {
                let mut cmd = Command::new(&executable);
                crate::compose_child_path(&mut cmd, &executable);
                cmd.arg("models");
                cmd.stdout(Stdio::piped()).stderr(Stdio::null());
                if let Ok(Ok(output)) = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output()).await
                    && output.status.success()
                    && let Ok(text) = String::from_utf8(output.stdout)
                {
                    let parsed = catalog::parse_models(&text);
                    if !parsed.is_empty() {
                        return Ok(parsed);
                    }
                }
                Ok(catalog::static_models())
            })
            .await
            .cloned()
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        Ok(vec![
            SlashCommand {
                name: "goal".into(),
                description: "Run a long-running autonomous task with extra thoroughness".into(),
                input_hint: Some("<goal instruction>".into()),
            },
            SlashCommand {
                name: "schedule".into(),
                description: "Schedule a one-shot timer or recurring cron job".into(),
                input_hint: Some("<schedule instruction>".into()),
            },
            SlashCommand {
                name: "browser".into(),
                description: "Web browsing, search, and web application testing".into(),
                input_hint: Some("<url or search task>".into()),
            },
            SlashCommand {
                name: "grill-me".into(),
                description: "Interactive architectural interview to clarify design decisions".into(),
                input_hint: None,
            },
            SlashCommand {
                name: "learn".into(),
                description: "Persist lessons, behavioral rules, or setup for future tasks".into(),
                input_hint: Some("<learning note>".into()),
            },
        ])
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<futures::stream::BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>
    {
        let executable = self.resolve_executable()?;
        let mut command = Command::new(&executable);
        crate::compose_child_path(&mut command, &executable);
        command
            .arg("-p")
            .arg(&request.prompt)
            .args(["--output-format", "stream-json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if !request.cwd.is_empty() {
            command.current_dir(&request.cwd);
            // Antigravity CLI uses --add-dir to track active workspace directories.
            // Without this flag, agy warns that no workspace is active.
            command.arg("--add-dir").arg(&request.cwd);
        }
        if let Some(conversation) = request.resume.as_deref().filter(|id| !id.is_empty()) {
            command.args(["--conversation", conversation]);
        }
        if let Some(model) = request
            .model
            .as_deref()
            .filter(|model| *model != catalog::default_model())
        {
            command.args(["--model", model]);
        }
        if let Some(effort) = catalog::to_effort(request.reasoning) {
            command.args(["--effort", effort]);
        }
        if request.sandbox == komet_proto::SandboxLevel::ReadOnly {
            command.arg("--sandbox");
        }
        // agy in non-interactive print mode (-p) cannot prompt for permissions over stdio;
        // any tool requiring review is auto-denied by agy causing premature cancellation.
        // Therefore --dangerously-skip-permissions is required to enable tool usage in normal runs.
        command.arg("--dangerously-skip-permissions");

        let mut child = command.spawn().map_err(HarnessError::Io)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("agy stdout unavailable".into()))?;
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::channel(128);

        let stderr_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let stderr_lines_clone = std::sync::Arc::clone(&stderr_lines);
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut lock = stderr_lines_clone.lock().unwrap();
                    if lock.len() >= 20 {
                        lock.remove(0);
                    }
                    lock.push(line);
                }
            });
        }

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut saw_text = false;
            let mut session_id: Option<String> = None;
            let mut finished = false;
            let mut seen_tools = std::collections::HashSet::new();
            let mut last_diagnostic: Option<String> = None;
            loop {
                tokio::select! {
                    _ = controls.interrupt.cancelled() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        let _ = tx.send(Ok(AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            result: None,
                            error: None,
                            session_id: session_id.clone(),
                            reason: Some(komet_proto::DoneReason::UserRequested),
                        })).await;
                        finished = true;
                        break;
                    }
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => {
                            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                                let trimmed = line.trim();
                                if !trimmed.is_empty() {
                                    last_diagnostic = Some(trimmed.to_string());
                                }
                                continue;
                            };
                            match frame.get("event").and_then(Value::as_str) {
                                Some("init") => {
                                    session_id = frame.get("conversation_id").and_then(Value::as_str).map(str::to_owned);
                                    let init = frame.get("init").unwrap_or(&Value::Null);
                                    if tx.send(Ok(AgentEvent::SessionStarted {
                                        harness: HarnessId::Antigravity,
                                        model: init.get("model").and_then(Value::as_str).unwrap_or(catalog::default_model()).to_owned(),
                                        tools: init.get("tools").and_then(Value::as_array).map(|tools| tools.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default(),
                                        cwd: init.get("cwd").and_then(Value::as_str).unwrap_or("").to_owned(),
                                        session_id: session_id.clone().unwrap_or_default(),
                                        assistant_message_id: uuid::Uuid::new_v4().to_string(),
                                    })).await.is_err() { break; }
                                }
                                Some("step_update") => {
                                    let step = frame.get("step_update").unwrap_or(&Value::Null);
                                    let text = step.get("text_delta")
                                        .or_else(|| step.get("delta"))
                                        .or_else(|| step.get("text"))
                                        .and_then(Value::as_str)
                                        .filter(|t| !t.is_empty());
                                    if let Some(text) = text {
                                        saw_text = true;
                                        if tx.send(Ok(AgentEvent::TextDelta { text: text.to_owned() })).await.is_err() { break; }
                                    }
                                    let is_tool = step.get("step_type").and_then(Value::as_str) == Some("tool")
                                        || step.get("tool_name").is_some()
                                        || step.get("tool_info").is_some();
                                    if is_tool {
                                        let step_idx = step.get("step_index").and_then(Value::as_u64).unwrap_or_default();
                                        let id = format!("agy-step-{step_idx}");
                                        let info = step.get("tool_info").unwrap_or(&Value::Null);
                                        let tool_name = info.get("name").or_else(|| step.get("tool_name")).and_then(Value::as_str).unwrap_or("tool");
                                        let params = info.get("parameters")
                                            .or_else(|| step.get("parameters"))
                                            .or_else(|| info.get("params"))
                                            .or_else(|| step.get("params"));
                                        let state = step.get("state").and_then(Value::as_str).unwrap_or("");

                                        // Emit ToolCall only once per tool id
                                        if seen_tools.insert(id.clone()) {
                                            // Bridge interactive questions if present
                                            if tool_name == "ask_question"
                                                && let Some(params) = params
                                                && let Some(questions) = normalize::extract_questions(params) {
                                                    let rx = (controls.request_input)(questions);
                                                    tokio::spawn(async move {
                                                        let _ = rx.await;
                                                    });
                                                }

                                            let call = normalize::normalize_tool_call(tool_name, params);
                                            if tx.send(Ok(AgentEvent::ToolCall { id: id.clone(), call })).await.is_err() { break; }
                                        }

                                        // Settle tool on DONE or ERROR
                                        if state == "DONE" || state == "ERROR" {
                                            let is_error = state == "ERROR" || info.get("error").is_some() || step.get("error").is_some();
                                            let output = info.get("output")
                                                .or_else(|| step.get("output"))
                                                .or_else(|| info.get("result"))
                                                .or_else(|| step.get("result"))
                                                .and_then(Value::as_str)
                                                .map(str::to_owned)
                                                .or_else(|| {
                                                    info.get("error").or_else(|| step.get("error")).map(|err| {
                                                        if let Some(msg) = err.get("message").and_then(Value::as_str) {
                                                            msg.to_owned()
                                                        } else if let Some(s) = err.as_str() {
                                                            s.to_owned()
                                                        } else {
                                                            err.to_string()
                                                        }
                                                    })
                                                });
                                            let diff: Option<komet_proto::ToolDiff> = info.get("diff")
                                                .or_else(|| step.get("diff"))
                                                .and_then(|d| {
                                                    if let Some(s) = d.as_str() {
                                                        let path = info.get("path")
                                                            .or_else(|| info.get("TargetFile"))
                                                            .or_else(|| step.get("path"))
                                                            .and_then(Value::as_str)
                                                            .unwrap_or_default()
                                                            .to_string();
                                                        Some(komet_proto::ToolDiff {
                                                            path,
                                                            old_text: None,
                                                            new_text: s.to_string(),
                                                        })
                                                    } else {
                                                        serde_json::from_value(d.clone()).ok()
                                                    }
                                                });
                                            if tx.send(Ok(AgentEvent::ToolResult { id, is_error, output, diff })).await.is_err() { break; }
                                        }
                                    }
                                    if let Some(usage) = step.get("usage")
                                        && tx.send(Ok(usage_event(usage))).await.is_err() { break; }
                                }
                                Some("result") => {
                                    let result = frame.get("result").unwrap_or(&Value::Null);
                                    session_id = result.get("conversation_id").and_then(Value::as_str).map(str::to_owned).or(session_id);
                                    let response_text = result.get("response").and_then(Value::as_str);
                                    if !saw_text
                                        && let Some(text) = response_text.filter(|text| !text.is_empty())
                                        && tx.send(Ok(AgentEvent::TextDelta { text: text.to_owned() })).await.is_err() { break; }
                                    if let Some(usage) = result.get("usage")
                                        && tx.send(Ok(usage_event(usage))).await.is_err() { break; }

                                    let status_str = result.get("status").and_then(Value::as_str).unwrap_or("");
                                    let success = status_str == "SUCCESS";

                                    let mut error = result.get("error").and_then(Value::as_str).map(str::to_owned);
                                    if !success && error.is_none() {
                                        let err_tail = {
                                            let l = stderr_lines.lock().unwrap();
                                            if !l.is_empty() {
                                                Some(l.join("\n"))
                                            } else {
                                                None
                                            }
                                        };
                                        error = err_tail.or_else(|| last_diagnostic.clone()).or_else(|| {
                                            Some(format!("Antigravity ended with status: {status_str}"))
                                        });
                                    }

                                    let event = AgentEvent::Done {
                                        status: if success { DoneStatus::Completed } else { DoneStatus::Errored },
                                        result: response_text.map(str::to_owned),
                                        error,
                                        session_id: session_id.clone(),
                                        reason: None,
                                    };
                                    let _ = tx.send(Ok(event)).await;
                                    finished = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        Ok(None) => break,
                        Err(error) => { let _ = tx.send(Err(HarnessError::Io(error))).await; break; }
                    }
                }
            }
            if !finished && !tx.is_closed() {
                let status = child.wait().await.ok();
                let err_tail = {
                    let l = stderr_lines.lock().unwrap();
                    if !l.is_empty() {
                        Some(l.join("\n"))
                    } else {
                        None
                    }
                };
                let detail = err_tail
                    .or(last_diagnostic)
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default();
                let _ = tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(format!(
                            "Antigravity CLI ended before returning a result ({}){}",
                            crate::describe_exit(status),
                            detail
                        )),
                        session_id,
                        reason: None,
                    }))
                    .await;
            }
        });
        Ok(stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed())
    }
}
