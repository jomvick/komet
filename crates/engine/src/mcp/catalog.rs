use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::composio::{ComposioSession, ComposioTool, is_read_action_slug};
use komet_proto::{
    TerminalCommand, TerminalDelta, TerminalInfo, TerminalScreen, TerminalTail,
};

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpaceSummary {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
}

/// Which slice of a terminal an agent asked to read (`terminal_read`'s `angle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAngle {
    Screen,
    Tail {
        lines: usize,
    },
    Delta {
        since_seq: u64,
    },
    /// The newest OSC 133 command block: command, cwd, output, exit code.
    /// Refuses (loudly, at the host) when the shell never emitted markers —
    /// guessing where the prompt was is the silent lie this forbids.
    LastCommand,
}

/// One terminal read answered by the host BEFORE the catalog's byte cap. The
/// engine's gate (agent visibility, then tracking) is what produced — or
/// refused — it; the host reports refusals as `String` errors so the MCP
/// layer surfaces them verbatim.
#[derive(Debug, Clone)]
pub enum TerminalRead {
    Screen(TerminalScreen),
    Tail(TerminalTail),
    Delta(TerminalDelta),
    Command(TerminalCommand),
}

#[async_trait::async_trait]
pub trait McpHost: Send + Sync {
    fn list_sessions(&self) -> Vec<SessionSummary>;
    fn session_status(&self, id: &str) -> Option<SessionSummary>;
    fn list_spaces(&self) -> Vec<SpaceSummary>;
    /// Every terminal, readable by agents (read-only: agents can never
    /// write to, open or close a terminal).
    fn list_terminals_for_agents(&self) -> Vec<TerminalInfo>;
    /// One read; `Err(message)` carries the engine's refusal (unknown
    /// terminal, no OSC 133 markers) up to the agent verbatim.
    fn read_terminal_for_agents(
        &self,
        terminal_id: &str,
        angle: &TerminalAngle,
    ) -> Result<TerminalRead, String>;
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub readonly: bool,
    composio_slug: Option<String>,
}

fn obj_schema(props: &[(&str, &str)], required: &[&str]) -> serde_json::Value {
    let properties: serde_json::Map<_, _> = props
        .iter()
        .map(|(k, t)| (k.to_string(), serde_json::json!({"type": t})))
        .collect();
    serde_json::json!({"type": "object", "properties": properties,
        "required": required, "additionalProperties": false})
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("missing required argument: {0}")]
    MissingArg(String),
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),
}

pub struct McpCatalog {
    tools: HashMap<String, ToolDef>,
    order: Vec<String>,
    host: Arc<dyn McpHost>,
    composio: Option<ComposioSession>,
}

impl McpCatalog {
    pub fn komet_default(host: Arc<dyn McpHost>) -> Self {
        let defs = vec![
            ToolDef {
                name: "list_sessions".into(),
                title: "List sessions".into(),
                description: "List recent chat sessions with id, title and status.".into(),
                input_schema: obj_schema(&[], &[]),
                readonly: true,
                composio_slug: None,
            },
            ToolDef {
                name: "get_session_status".into(),
                title: "Get session status".into(),
                description: "Return id, title and status for one session.".into(),
                input_schema: obj_schema(&[("sessionId", "string")], &["sessionId"]),
                readonly: true,
                composio_slug: None,
            },
            ToolDef {
                name: "list_spaces".into(),
                title: "List spaces".into(),
                description: "List workspace spaces with id, path and name.".into(),
                input_schema: obj_schema(&[], &[]),
                readonly: true,
                composio_slug: None,
            },
            ToolDef {
                name: "terminal_list".into(),
                title: "List terminals".into(),
                description: "List every Komet terminal with id, title, cwd, shell, grid size and seq. Read-only: agents can read terminals but never write to, open or close them.".into(),
                input_schema: obj_schema(&[], &[]),
                readonly: true,
                composio_slug: None,
            },
            ToolDef {
                name: "terminal_read".into(),
                title: "Read a terminal".into(),
                description: "Read one  terminal in clear text: angle \"screen\" (current grid — the read for a full-screen app), \"tail\" (last `lines` lines), \"delta\" (what appeared after `sinceSeq`, a seq a previous read reported), \"last_command\" (the newest OSC 133 command block: command, cwd, output, exit code — refuses if the shell emits no markers). Output is capped keeping the END; an unknown terminal errors, never answers empty.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "terminalId": {"type": "string"},
                        "angle": {"type": "string", "enum": ["screen", "tail", "delta", "last_command"]},
                        "lines": {"type": "number"},
                        "sinceSeq": {"type": "number"},
                    },
                    "required": ["terminalId", "angle"],
                    "additionalProperties": false,
                }),
                readonly: true,
                composio_slug: None,
            },
        ];
        let order = defs
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        Self {
            tools: defs
                .into_iter()
                .map(|definition| (definition.name.clone(), definition))
                .collect(),
            order,
            host,
            composio: None,
        }
    }

    pub fn with_composio(mut self, session: ComposioSession, tools: Vec<ComposioTool>) -> Self {
        self.composio = Some(session);
        for tool in tools {
            let name = format!("composio_{}", tool.slug.to_ascii_lowercase());
            if self.tools.contains_key(&name) {
                tracing::warn!(tool = %tool.slug, "duplicate Composio MCP tool skipped");
                continue;
            }
            self.order.push(name.clone());
            self.tools.insert(
                name.clone(),
                ToolDef {
                    title: tool.slug.clone(),
                    description: tool.description,
                    input_schema: tool.input_schema,
                    readonly: is_read_action_slug(&tool.slug),
                    composio_slug: Some(tool.slug),
                    name,
                },
            );
        }
        self
    }

    pub fn list(&self) -> Vec<ToolDef> {
        // Definition order (matches the acceptance test) — NOT sorted.
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name).cloned())
            .collect()
    }

    pub fn is_readonly(&self, name: &str, arguments: &serde_json::Value) -> bool {
        if name.eq_ignore_ascii_case("composio_composio_multi_execute_tool")
            || name.eq_ignore_ascii_case("composio_multi_execute_tool")
        {
            let mut slugs = Vec::new();
            collect_tool_slugs(arguments, &mut slugs);
            return !slugs.is_empty() && slugs.iter().all(|slug| is_read_action_slug(slug));
        }
        self.tools
            .get(name)
            .is_some_and(|definition| definition.readonly)
    }

    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, CatalogError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| CatalogError::Unknown(name.into()))?;
        match tool.name.as_str() {
            "list_sessions" => Ok(serde_json::json!({"sessions": self.host.list_sessions()})),
            "get_session_status" => {
                let id = input
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| CatalogError::MissingArg("sessionId".into()))?;
                Ok(self
                    .host
                    .session_status(id)
                    .map(|s| serde_json::json!({"session": s}))
                    .unwrap_or_else(|| serde_json::json!({"session": null})))
            }
            "list_spaces" => Ok(serde_json::json!({"spaces": self.host.list_spaces()})),
            "terminal_list" => Ok(serde_json::json!({
                "terminals": self.host.list_terminals_for_agents()
            })),
            "terminal_read" => {
                let terminal_id = input
                    .get("terminalId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| CatalogError::MissingArg("terminalId".into()))?;
                let angle = match input.get("angle").and_then(|v| v.as_str()) {
                    None => return Err(CatalogError::MissingArg("angle".into())),
                    Some("screen") => TerminalAngle::Screen,
                    Some("tail") => TerminalAngle::Tail {
                        lines: clamp_tail_lines(input.get("lines").and_then(|v| v.as_u64())),
                    },
                    Some("delta") => {
                        let since_seq = input
                            .get("sinceSeq")
                            .and_then(|v| v.as_u64())
                            .ok_or_else(|| CatalogError::MissingArg("sinceSeq".into()))?;
                        TerminalAngle::Delta { since_seq }
                    }
                    // The honest slice: the host refuses when the shell never
                    // emitted markers (guessing the prompt boundary is the
                    // silent lie this forbids).
                    Some("last_command") => TerminalAngle::LastCommand,
                    Some(other) => {
                        return Err(CatalogError::ExecutionFailed(format!(
                            "unknown angle {other:?}; expected screen | tail | delta | last_command"
                        )));
                    }
                };
                let read = self
                    .host
                    .read_terminal_for_agents(terminal_id, &angle)
                    .map_err(CatalogError::ExecutionFailed)?;
                Ok(terminal_read_json(read))
            }
            _ => match (&tool.composio_slug, &self.composio) {
                (Some(slug), Some(session)) => session
                    .execute(slug, input)
                    .await
                    .map_err(|error| CatalogError::ExecutionFailed(error.to_string())),
                _ => Err(CatalogError::Unknown(name.into())),
            },
        }
    }
}

/// Char budget a single `terminal_read` reply may carry: the MCP endpoint runs
/// a 30s `TOOL_TIMEOUT` with four concurrent calls, so a read must stay bounded
/// — never a 1 MB dump. On overflow the END is kept (terminal answers live at
/// the bottom: the latest output, the error, the prompt).
pub(crate) const MAX_TERMINAL_READ_CHARS: usize = 40_000;
/// An agent cannot ask for more tail lines than the tracked mirror holds.
pub(crate) const MAX_TERMINAL_TAIL_LINES: usize = 1_000;
pub(crate) const DEFAULT_TERMINAL_TAIL_LINES: usize = 200;
fn clamp_tail_lines(requested: Option<u64>) -> usize {
    requested.map_or(DEFAULT_TERMINAL_TAIL_LINES, |v| {
        (v.min(MAX_TERMINAL_TAIL_LINES as u64) as usize).max(1)
    })
}

/// The wire shape of one capped terminal read. Field names match the proto
/// transport types (camelCase); `truncated` folds the engine's own flag (a
/// dropped journal range) with this cap having bitten.
fn terminal_read_json(read: TerminalRead) -> serde_json::Value {
    match read {
        TerminalRead::Screen(screen) => {
            let (lines, capped) = cap_lines_tail(screen.lines, MAX_TERMINAL_READ_CHARS);
            serde_json::json!({
                "angle": "screen",
                "terminalId": screen.terminal_id,
                "cols": screen.cols,
                "rows": screen.rows,
                "seq": screen.seq,
                "title": screen.title,
                "lines": lines,
                "truncated": capped,
            })
        }
        TerminalRead::Tail(tail) => {
            let (lines, capped) = cap_lines_tail(tail.lines, MAX_TERMINAL_READ_CHARS);
            serde_json::json!({
                "angle": "tail",
                "terminalId": tail.terminal_id,
                "seq": tail.seq,
                "requested": tail.requested,
                "lines": lines,
                "truncated": capped,
            })
        }
        TerminalRead::Delta(delta) => {
            let (lines, capped) = cap_lines_tail(delta.lines, MAX_TERMINAL_READ_CHARS);
            serde_json::json!({
                "angle": "delta",
                "terminalId": delta.terminal_id,
                "fromSeq": delta.from_seq,
                "throughSeq": delta.through_seq,
                "needsSnapshot": delta.needs_snapshot,
                "truncated": delta.truncated || capped,
                "lines": lines,
            })
        }
        TerminalRead::Command(command) => {
            let (lines, capped) = cap_lines_tail(command.lines, MAX_TERMINAL_READ_CHARS);
            serde_json::json!({
                "angle": "last_command",
                "terminalId": command.terminal_id,
                "status": command.status,
                "command": command.command,
                "cwd": command.cwd,
                "exitCode": command.exit_code,
                "markerBacked": command.marker_backed,
                "seq": command.seq,
                "truncated": capped,
                "lines": lines,
            })
        }
    }
}

/// Keep the trailing lines that fit in `budget` chars, oldest dropped first;
/// when not even one line fits, keep the end of the newest line. Returns the
/// kept lines and whether anything was dropped.
fn cap_lines_tail(lines: Vec<String>, budget: usize) -> (Vec<String>, bool) {
    if lines.is_empty() {
        return (lines, false);
    }
    let mut used = 0usize;
    let mut keep_from = lines.len();
    for (ix, line) in lines.iter().enumerate().rev() {
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        keep_from = ix;
    }
    if keep_from == 0 {
        return (lines, false);
    }
    if keep_from == lines.len() {
        // Not even the newest line fits whole: its end still says something.
        let last = lines.last().expect("non-empty");
        let count = last.chars().count();
        let tail = last.chars().skip(count.saturating_sub(budget)).collect();
        return (vec![tail], true);
    }
    (lines[keep_from..].to_vec(), true)
}

fn collect_tool_slugs(value: &serde_json::Value, output: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, nested) in object {
                if matches!(key.as_str(), "tool_slug" | "toolSlug" | "slug") {
                    if let Some(slug) = nested.as_str() {
                        output.push(slug.to_string());
                    }
                } else {
                    collect_tool_slugs(nested, output);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_tool_slugs(item, output);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use komet_proto::{TerminalDelta, TerminalInfo, TerminalScreen, TerminalTail};

    fn info(id: &str) -> TerminalInfo {
        TerminalInfo {
            terminal_id: id.into(),
            title: Some("cargo watch".into()),
            cwd: "/r".into(),
            shell: "sh".into(),
            cols: 80,
            rows: 24,
            seq: 7,
            tracked: true,
            exited: false,
        }
    }

    /// Refuses every read the way the engine does for an unknown terminal
    /// (the host forwards the message verbatim) and lists nothing.
    #[derive(Default)]
    struct FakeHost;

    #[async_trait::async_trait]
    impl McpHost for FakeHost {
        fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }

        fn session_status(&self, _id: &str) -> Option<SessionSummary> {
            None
        }

        fn list_spaces(&self) -> Vec<SpaceSummary> {
            vec![SpaceSummary {
                id: "s1".into(),
                path: "/r".into(),
                name: None,
            }]
        }

        fn list_terminals_for_agents(&self) -> Vec<TerminalInfo> {
            vec![]
        }

        fn read_terminal_for_agents(
            &self,
            terminal_id: &str,
            _angle: &TerminalAngle,
        ) -> Result<TerminalRead, String> {
            Err(format!(
                "Unknown terminal {terminal_id}"
            ))
        }




    }

    /// One terminal, answering reads with canned text and recording
    /// whether (and how) the host was consulted.
    struct GrantedHost {
        read_calls: AtomicUsize,
        last_angle: Mutex<Option<TerminalAngle>>,
        tail_lines: usize,
        line_len: usize,
        /// Whether this terminal's shell emits OSC 133 markers. Without them
        /// `last_command` must refuse loudly, never guess.
        markers: bool,
    }

    impl GrantedHost {
        fn new() -> Self {
            Self {
                read_calls: AtomicUsize::new(0),
                last_angle: Mutex::new(None),
                tail_lines: 8,
                line_len: 10,
                markers: true,
            }
        }

        fn without_markers() -> Self {
            let mut host = Self::new();
            host.markers = false;
            host
        }
    }

    #[async_trait::async_trait]
    impl McpHost for GrantedHost {
        fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }

        fn session_status(&self, _id: &str) -> Option<SessionSummary> {
            None
        }

        fn list_spaces(&self) -> Vec<SpaceSummary> {
            vec![]
        }

        fn list_terminals_for_agents(&self) -> Vec<TerminalInfo> {
            vec![info("t-visible")]
        }

        fn read_terminal_for_agents(
            &self,
            terminal_id: &str,
            angle: &TerminalAngle,
        ) -> Result<TerminalRead, String> {
            self.read_calls.fetch_add(1, Ordering::Relaxed);
            *self.last_angle.lock().unwrap() = Some(angle.clone());
            let lines: Vec<String> = (0..self.tail_lines)
                .map(|ix| format!("line-{ix:03}-{}", "x".repeat(self.line_len)))
                .collect();
            Ok(match angle {
                TerminalAngle::Screen => TerminalRead::Screen(TerminalScreen {
                    terminal_id: terminal_id.into(),
                    cols: 80,
                    rows: 24,
                    seq: 9,
                    title: None,
                    lines,
                }),
                TerminalAngle::Tail { lines: asked } => TerminalRead::Tail(TerminalTail {
                    terminal_id: terminal_id.into(),
                    seq: 9,
                    requested: *asked,
                    lines,
                }),
                TerminalAngle::Delta { since_seq } => TerminalRead::Delta(TerminalDelta {
                    terminal_id: terminal_id.into(),
                    from_seq: *since_seq,
                    through_seq: *since_seq + 2,
                    lines,
                    needs_snapshot: false,
                    truncated: false,
                }),
                TerminalAngle::LastCommand => {
                    if !self.markers {
                        return Err(
                            "the `last_command` angle needs command markers (OSC 133), which this shell never emitted; use `tail` or `delta`".into(),
                        );
                    }
                    TerminalRead::Command(TerminalCommand {
                        terminal_id: terminal_id.into(),
                        status: "done".into(),
                        command: Some("make test".into()),
                        cwd: Some("/repo".into()),
                        exit_code: Some(2),
                        marker_backed: true,
                        seq: 9,
                        lines: lines.clone(),
                    })
                }
            })
        }




    }

    #[tokio::test]
    async fn catalog_lists_default_tools_and_executes() {
        let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
        let names: Vec<_> = catalog.list().into_iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            vec![
                "list_sessions".to_string(),
                "get_session_status".to_string(),
                "list_spaces".to_string(),
                "terminal_list".to_string(),
                "terminal_read".to_string(),
            ]
        );
        let v = catalog
            .execute("list_spaces", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"spaces": [{"id": "s1", "path": "/r", "name": null}]})
        );
        assert!(
            catalog
                .execute("nope", serde_json::json!({}))
                .await
                .is_err()
        );
    }

    /// Agents read terminals but never write: every tool is read-only and no
    /// tool can open, close, type into or run anything in a terminal.
    #[tokio::test]
    async fn catalog_has_no_terminal_write_tools() {
        let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
        for tool in catalog.list() {
            assert!(tool.readonly, "{} must be read-only", tool.name);
            for forbidden in ["open", "close", "send", "input", "run", "set", "grant"] {
                assert!(
                    !tool.name.contains(forbidden),
                    "tool {} would write to a terminal",
                    tool.name
                );
            }
        }
        let terminal_tools: Vec<_> = catalog
            .list()
            .into_iter()
            .map(|t| t.name)
            .filter(|name| name.starts_with("terminal_"))
            .collect();
        assert_eq!(terminal_tools, vec!["terminal_list", "terminal_read"]);
        let err = catalog
            .execute("terminal_run", serde_json::json!({"command": "ls"}))
            .await;
        assert!(err.is_err(), "terminal_run must not exist");
    }

    #[tokio::test]
    async fn terminal_list_answers_with_the_terminals() {
        let catalog = McpCatalog::komet_default(Arc::new(GrantedHost::new()));
        let v = catalog
            .execute("terminal_list", serde_json::json!({}))
            .await
            .expect("terminal_list");
        let terminals = v["terminals"].as_array().expect("array");
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0]["terminalId"], "t-visible");
        assert_eq!(terminals[0]["title"], "cargo watch");
        assert_eq!(terminals[0]["cwd"], "/r");
        assert_eq!(terminals[0]["cols"], 80);
        assert_eq!(terminals[0]["rows"], 24);
        assert_eq!(terminals[0]["tracked"], true);
    }

    /// The guard, end to end through the tool: an unknown
    /// terminal is refused with the engine's message, never answered with an
    /// empty string (a silent empty read would be the no-op this forbids).
    #[tokio::test]
    async fn terminal_read_surfaces_the_engine_refusal() {
        let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
        for angle in ["screen", "tail", "delta"] {
            let mut args = serde_json::json!({"terminalId": "t-hidden", "angle": angle});
            if angle == "delta" {
                args["sinceSeq"] = serde_json::json!(3);
            }
            let err = catalog
                .execute("terminal_read", args)
                .await
                .expect_err("invisible terminal must be refused, not echoed empty");
            let msg = err.to_string();
            assert!(msg.contains("Unknown terminal"), "{msg}");
        }
    }

    #[tokio::test]
    async fn terminal_read_screen_returns_the_capped_grid() {
        let host = Arc::new(GrantedHost::new());
        let catalog = McpCatalog::komet_default(host.clone());
        let v = catalog
            .execute(
                "terminal_read",
                serde_json::json!({"terminalId": "t-visible", "angle": "screen"}),
            )
            .await
            .expect("screen read");
        assert_eq!(v["angle"], "screen");
        assert_eq!(v["seq"], 9);
        assert_eq!(v["lines"].as_array().map(Vec::len), Some(8));
        assert_eq!(v["truncated"], false);
        assert_eq!(
            *host.last_angle.lock().unwrap(),
            Some(TerminalAngle::Screen),
            "the screen angle must reach the gated engine read"
        );
    }

    /// A read never ships 1 MB: over budget the END of the answer is kept (the
    /// newest lines, and the tail of a single huge one).
    #[tokio::test]
    async fn terminal_read_truncation_keeps_the_end() {
        let mut host = GrantedHost::new();
        host.tail_lines = 600;
        host.line_len = 200;
        let host = Arc::new(host);
        let catalog = McpCatalog::komet_default(host.clone());
        let v = catalog
            .execute(
                "terminal_read",
                serde_json::json!({"terminalId": "t-visible", "angle": "tail", "lines": 600}),
            )
            .await
            .expect("tail read");
        let lines = v["lines"].as_array().expect("lines");
        let total: usize = lines
            .iter()
            .map(|l| l.as_str().unwrap().chars().count())
            .sum();
        assert!(total <= MAX_TERMINAL_READ_CHARS, "{total} over budget");
        assert_eq!(v["truncated"], true);
        assert!(
            lines
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("line-599-"),
            "the newest line must survive"
        );
        assert!(
            !lines[0].as_str().unwrap().starts_with("line-000"),
            "oldest dropped"
        );
    }

    #[test]
    fn cap_lines_tail_keeps_the_end_of_one_giant_line() {
        let huge = format!("head{}TAIL", "z".repeat(MAX_TERMINAL_READ_CHARS));
        let (kept, capped) = cap_lines_tail(vec![huge], MAX_TERMINAL_READ_CHARS);
        assert!(capped);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].ends_with("TAIL"));
        assert!(kept[0].chars().count() <= MAX_TERMINAL_READ_CHARS);
        // Under budget: untouched.
        let (kept, capped) = cap_lines_tail(vec!["a".into(), "b".into()], 10);
        assert_eq!(kept, vec!["a".to_string(), "b".to_string()]);
        assert!(!capped);
    }

    /// With markers on the terminal, `last_command` is a REAL angle: command,
    /// cwd, output and a NON-ZERO exit code travel back through the tool.
    #[tokio::test]
    async fn terminal_read_last_command_answers_the_block() {
        let host = Arc::new(GrantedHost::new());
        let catalog = McpCatalog::komet_default(host.clone());
        let v = catalog
            .execute(
                "terminal_read",
                serde_json::json!({"terminalId": "t-visible", "angle": "last_command"}),
            )
            .await
            .expect("last_command read");
        assert_eq!(v["angle"], "last_command");
        assert_eq!(v["status"], "done");
        assert_eq!(v["command"], "make test");
        assert_eq!(v["cwd"], "/repo");
        assert_eq!(v["exitCode"], 2, "a non-zero exit must not be laundered");
        assert_eq!(v["markerBacked"], true);
        assert_eq!(v["lines"].as_array().map(Vec::len), Some(8));
        assert_eq!(
            *host.last_angle.lock().unwrap(),
            Some(TerminalAngle::LastCommand),
            "the angle must reach the gated engine read"
        );
    }

    /// Without markers the same angle still refuses loudly (nothing is
    /// fabricated from the prompt geometry).
    #[tokio::test]
    async fn terminal_read_last_command_refuses_without_markers() {
        let host = Arc::new(GrantedHost::without_markers());
        let catalog = McpCatalog::komet_default(host.clone());
        let err = catalog
            .execute(
                "terminal_read",
                serde_json::json!({"terminalId": "t-visible", "angle": "last_command"}),
            )
            .await
            .expect_err("must refuse, not fabricate a command boundary");
        assert!(err.to_string().contains("command markers"), "{err}");
        // The refusal went THROUGH the gate path (the host was consulted and
        // answered honestly), not a silent empty success.
        assert_eq!(host.read_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn terminal_read_delta_carries_only_the_answered_range() {
        let host = Arc::new(GrantedHost::new());
        let catalog = McpCatalog::komet_default(host.clone());
        let v = catalog
            .execute(
                "terminal_read",
                serde_json::json!({"terminalId": "t-visible", "angle": "delta", "sinceSeq": 5}),
            )
            .await
            .expect("delta read");
        assert_eq!(v["fromSeq"], 5);
        assert_eq!(v["throughSeq"], 7);
        assert_eq!(v["needsSnapshot"], false);
        assert_eq!(v["truncated"], false);
        assert_eq!(
            *host.last_angle.lock().unwrap(),
            Some(TerminalAngle::Delta { since_seq: 5 })
        );
        // Missing range anchor: refused with a missing-argument error.
        let err = catalog
            .execute(
                "terminal_read",
                serde_json::json!({"terminalId": "t-visible", "angle": "delta"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::MissingArg(arg) if arg == "sinceSeq"));
        let err = catalog
            .execute("terminal_read", serde_json::json!({"angle": "screen"}))
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::MissingArg(arg) if arg == "terminalId"));
    }

    /// A tail request is clamped to what the tracked mirror can honestly hold.
    #[test]
    fn tail_requests_are_clamped() {
        assert_eq!(clamp_tail_lines(None), DEFAULT_TERMINAL_TAIL_LINES);
        assert_eq!(clamp_tail_lines(Some(50)), 50);
        assert_eq!(clamp_tail_lines(Some(0)), 1);
        assert_eq!(clamp_tail_lines(Some(u64::MAX)), MAX_TERMINAL_TAIL_LINES);
    }

    #[test]
    fn composio_mutations_require_permission_but_reads_do_not() {
        let catalog = McpCatalog::komet_default(Arc::new(FakeHost));
        assert!(catalog.is_readonly(
            "composio_composio_multi_execute_tool",
            &serde_json::json!({"tools": [{"tool_slug": "GMAIL_SEARCH_EMAILS"}]})
        ));
        assert!(!catalog.is_readonly(
            "composio_composio_multi_execute_tool",
            &serde_json::json!({"tools": [{"tool_slug": "GMAIL_SEND_EMAIL"}]})
        ));
        assert!(!catalog.is_readonly(
            "composio_composio_multi_execute_tool",
            &serde_json::json!({"tools": []})
        ));
    }
}
