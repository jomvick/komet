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
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode, ToolCall,
};

use crate::{Harness, HarnessError, RunControls};

pub struct AntigravityHarness {
    executable: Option<PathBuf>,
}

impl AntigravityHarness {
    pub fn new() -> Self {
        Self { executable: None }
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
        }
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

fn effort(level: Option<ReasoningLevel>) -> Option<&'static str> {
    match level {
        Some(ReasoningLevel::Minimal | ReasoningLevel::Low) => Some("low"),
        Some(ReasoningLevel::Medium) => Some("medium"),
        Some(_) => Some("high"),
        None => None,
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
        &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        Ok(vec![Model {
            id: catalog::default_model().into(),
            label: "Antigravity default".into(),
            description: Some("Uses the model selected in Antigravity CLI".into()),
            reasoning_levels: self.reasoning_levels().to_vec(),
            options: Vec::new(),
        }])
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
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if !request.cwd.is_empty() {
            command.current_dir(&request.cwd);
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
        if let Some(effort) = effort(request.reasoning) {
            command.args(["--effort", effort]);
        }
        if request.auto_approve {
            command.arg("--dangerously-skip-permissions");
        }
        let mut child = command.spawn().map_err(HarnessError::Io)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("agy stdout unavailable".into()))?;
        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut saw_text = false;
            let mut session_id: Option<String> = None;
            let mut finished = false;
            loop {
                tokio::select! {
                    _ = controls.interrupt.cancelled() => {
                        let _ = child.start_kill();
                        let _ = tx.send(Ok(AgentEvent::Done { status: DoneStatus::Interrupted, result: None, error: None, session_id: session_id.clone() })).await;
                        break;
                    }
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => {
                            let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue; };
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
                                    if let Some(text) = step.get("text_delta").and_then(Value::as_str) {
                                        saw_text = true;
                                        if tx.send(Ok(AgentEvent::TextDelta { text: text.to_owned() })).await.is_err() { break; }
                                    }
                                    if step.get("step_type").and_then(Value::as_str) == Some("tool") {
                                        let id = format!("agy-step-{}", step.get("step_index").and_then(Value::as_u64).unwrap_or_default());
                                        let info = step.get("tool_info").unwrap_or(&Value::Null);
                                        let call = ToolCall::Unknown { name: info.get("name").or_else(|| step.get("tool_name")).and_then(Value::as_str).unwrap_or("tool").to_owned(), input: info.get("parameters").cloned() };
                                        if tx.send(Ok(AgentEvent::ToolCall { id: id.clone(), call })).await.is_err() { break; }
                                        if step.get("state").and_then(Value::as_str) == Some("DONE") {
                                            let is_error = info.get("error").is_some();
                                            if tx.send(Ok(AgentEvent::ToolResult { id, is_error, output: info.get("output").and_then(Value::as_str).map(str::to_owned), diff: None })).await.is_err() { break; }
                                        }
                                    }
                                    if let Some(usage) = step.get("usage")
                                        && tx.send(Ok(usage_event(usage))).await.is_err() { break; }
                                }
                                Some("result") => {
                                    let result = frame.get("result").unwrap_or(&Value::Null);
                                    session_id = result.get("conversation_id").and_then(Value::as_str).map(str::to_owned).or(session_id);
                                    if !saw_text
                                        && let Some(text) = result.get("response").and_then(Value::as_str).filter(|text| !text.is_empty())
                                            && tx.send(Ok(AgentEvent::TextDelta { text: text.to_owned() })).await.is_err() { break; }
                                    if let Some(usage) = result.get("usage")
                                        && tx.send(Ok(usage_event(usage))).await.is_err() { break; }
                                    let success = result.get("status").and_then(Value::as_str) == Some("SUCCESS");
                                    let event = AgentEvent::Done {
                                        status: if success { DoneStatus::Completed } else { DoneStatus::Errored },
                                        result: result.get("response").and_then(Value::as_str).map(str::to_owned),
                                        error: result.get("error").and_then(Value::as_str).map(str::to_owned),
                                        session_id: session_id.clone(),
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
                let _ = tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(format!(
                            "Antigravity CLI ended before returning a result ({})",
                            crate::describe_exit(status)
                        )),
                        session_id,
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
