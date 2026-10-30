//! ACP `session/update` → [`AgentEvent`] mapping (protocol v1, the line the
//! claude/codex adapters and Grok Build speak today).
//!
//! Tolerant by construction, like the codex normalizer: decoded from raw
//! [`Value`]s, unknown `sessionUpdate` kinds and content types map to nothing
//! (traced at debug, never silently), and missing fields degrade to empty
//! strings rather than errors. Wire shapes
//! verified against `agent-client-protocol-schema` 1.3.0 (`SessionUpdate` is
//! tagged `sessionUpdate`/snake_case; structs are camelCase; tool kinds and
//! statuses are snake_case).

use komet_proto::{
    AgentEvent, ContextUsageSource, ContextUsageStats, SlashCommand, TodoItem, ToolCall, ToolDiff,
    UserInputAnswer, UserInputQuestion,
};
use serde_json::Value;

/// Byte cap applied to tool output text at the harness boundary. The doc-side
/// fold applies its own (smaller) cap before anything persists; this one only
/// bounds what crosses the event stream.
pub(crate) const OUTPUT_CAP: usize = 16 * 1024;

/// Byte cap for each side of an inline diff crossing the event stream.
pub(crate) const DIFF_TEXT_CAP: usize = 64 * 1024;

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// Truncate on a char boundary, marking the cut so the UI can say "truncated".
pub(crate) fn cap_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push_str("\n… [truncated]");
    out
}

/// The text of a `ContentBlock` (`{type: "text", text}`); non-text blocks
/// (image, audio, resource, resource_link) render as nothing.
fn content_block_text(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| block.get("text").and_then(Value::as_str))
        .flatten()
}

/// A `ContentChunk`'s streamed text (`{content: {type: "text", ...}}`).
pub(crate) fn chunk_text(update: &Value) -> Option<String> {
    let text = content_block_text(update.get("content")?)?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// Joined text of a tool call's `content` array, capped; `None` when empty.
fn tool_output(update: &Value) -> Option<String> {
    let parts: Vec<&str> = update
        .get("content")?
        .as_array()?
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("content"))
        .filter_map(|c| content_block_text(c.get("content")?))
        .filter(|t| !t.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(cap_text(&parts.join("\n"), OUTPUT_CAP))
}

/// First `{type: "diff"}` entry of a tool call's `content` array.
fn tool_diff(update: &Value) -> Option<ToolDiff> {
    let diff = update
        .get("content")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("diff"))?;
    let path = str_field(diff, "path");
    if path.is_empty() {
        return None;
    }
    Some(ToolDiff {
        path,
        old_text: diff
            .get("oldText")
            .and_then(Value::as_str)
            .map(|t| cap_text(t, DIFF_TEXT_CAP)),
        new_text: cap_text(
            diff.get("newText").and_then(Value::as_str).unwrap_or(""),
            DIFF_TEXT_CAP,
        ),
    })
}

/// The grok-native tool name stamped on a tool call's `_meta` (`x.ai/tool`,
/// present on every grok tool_call — verified live, 1.0.4).
pub(crate) fn xai_tool_name(update: &Value) -> Option<&str> {
    update.get("_meta")?.get("x.ai/tool")?.get("name")?.as_str()
}

/// First location path (`locations: [{path, line?}]`), for read/edit calls.
fn first_location(update: &Value) -> Option<String> {
    let path = update.get("locations")?.as_array()?.first()?.get("path")?;
    path.as_str().filter(|p| !p.is_empty()).map(str::to_owned)
}

/// Cursor (and similar ACP agents) put a human summary in `title` — generic
/// labels ("Read File", "grep") before args arrive, or a markdown-wrapped
/// command (`` `ls -la` `` with inner backticks escaped as `\``). Those are
/// display strings, not typed arguments; using them as path/pattern/command
/// dumps the label (and its escapes) into the transcript chip.
fn is_placeholder_title(title: &str) -> bool {
    matches!(
        title.trim(),
        "grep"
            | "Find"
            | "Terminal"
            | "Read File"
            | "Edit File"
            | "Delete File"
            | "Web Search"
            | "Web Fetch"
            | "Codebase Search"
            | "Read TODOs"
            | "Update TODOs"
            | "Read Lints"
            | "Task: Subagent task"
            | "Subagent task"
            | "List MCP Resources"
            | "Fetch MCP Resource"
    )
}

/// Unwrap a single markdown code span used as an ACP exec title.
fn unwrap_command_title(title: &str) -> Option<String> {
    let inner = title.trim().strip_prefix('`')?.strip_suffix('`')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'`') {
            chars.next();
            out.push('`');
        } else {
            out.push(c);
        }
    }
    let out = out.trim();
    if out.is_empty() || is_placeholder_title(out) {
        None
    } else {
        Some(out.to_owned())
    }
}

fn arg_from_title(title: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() || is_placeholder_title(t) {
        None
    } else {
        Some(t.to_owned())
    }
}

/// Reduce an ACP tool call (kind + title + rawInput + locations + diff
/// content) to the typed [`ToolCall`] komet renders. Best-effort: agents vary
/// in how much structure they put in `rawInput`, so every arm has a fallback.
/// Title is only used when it looks like a real arg — never a placeholder
/// label or markdown-escaped summary.
fn typed_call(update: &Value) -> ToolCall {
    let kind = update
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("other");
    let title = str_field(update, "title");
    let raw = update.get("rawInput").filter(|v| !v.is_null());
    let raw_str = |key: &str| -> Option<String> {
        raw.and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match kind {
        "execute" => ToolCall::Exec {
            command: raw_str("command")
                .or_else(|| unwrap_command_title(&title))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_default(),
        },
        "read" => ToolCall::ReadFile {
            path: raw_str("path")
                .or_else(|| raw_str("file_path"))
                .or_else(|| raw_str("filePath"))
                .or_else(|| first_location(update))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_default(),
        },
        "edit" | "delete" | "move" => {
            // A diff pins down the file and shape; otherwise fall back to the
            // location/rawInput path with unknown content.
            if let Some(diff) = tool_diff(update) {
                if diff.old_text.is_none() {
                    ToolCall::WriteFile {
                        path: diff.path,
                        content: None,
                    }
                } else {
                    ToolCall::EditFile {
                        path: diff.path,
                        old_string: None,
                        new_string: None,
                    }
                }
            } else {
                match raw_str("path")
                    .or_else(|| raw_str("file_path"))
                    .or_else(|| raw_str("filePath"))
                    .or_else(|| first_location(update))
                {
                    Some(path) if kind == "edit" => ToolCall::EditFile {
                        path,
                        old_string: None,
                        new_string: None,
                    },
                    path => ToolCall::ApplyPatch { path },
                }
            }
        }
        "search" => {
            // Cursor web search is kind "search" + rawInput.searchTerm; grep
            // / glob / codebase search use pattern or query.
            if let Some(query) = raw_str("searchTerm") {
                ToolCall::WebSearch { query }
            } else {
                ToolCall::Search {
                    pattern: raw_str("pattern")
                        .or_else(|| raw_str("globPattern"))
                        .or_else(|| raw_str("query"))
                        .or_else(|| arg_from_title(&title))
                        .unwrap_or_default(),
                    path: raw_str("path"),
                }
            }
        }
        "fetch" => match raw_str("url") {
            Some(url) => ToolCall::WebFetch { url, prompt: None },
            None => ToolCall::WebSearch {
                query: raw_str("searchTerm")
                    .or_else(|| raw_str("query"))
                    .or_else(|| arg_from_title(&title))
                    .unwrap_or_default(),
            },
        },
        // Kindless (or unknown-kind) update carrying a diff: an edit in all
        // but name — some agents only attach the diff on the completion
        // update without repeating the kind.
        _ if tool_diff(update).is_some() => {
            let diff = tool_diff(update).expect("guarded");
            if diff.old_text.is_none() {
                ToolCall::WriteFile {
                    path: diff.path,
                    content: None,
                }
            } else {
                ToolCall::EditFile {
                    path: diff.path,
                    old_string: None,
                    new_string: None,
                }
            }
        }
        // Grok's subagent spawn: name the chip — and the subagent tab it
        // opens — after the task, matching the claude driver's "Agent: {d}"
        // (the bare tool name says nothing in a tab strip).
        _ if xai_tool_name(update) == Some("spawn_subagent") => ToolCall::Unknown {
            name: raw_str("description")
                .map(|d| format!("Agent: {d}"))
                .unwrap_or_else(|| "Agent".into()),
            input: raw.cloned(),
        },
        // opencode's subagent spawn (`task` tool — rawInput carries
        // description/prompt/subagent_type): same naming as grok's, so the
        // chip and its subagent tab say what the agent is doing.
        _ if raw.is_some_and(|r| r.get("subagent_type").is_some() && r.get("prompt").is_some()) => {
            ToolCall::Unknown {
                name: raw_str("description")
                    .map(|d| format!("Agent: {d}"))
                    .unwrap_or_else(|| "Agent".into()),
                input: raw.cloned(),
            }
        }
        // Its completion drops rawInput but keeps a title (the description),
        // which would re-type the chip to a bare Unknown and cost it the
        // Agent icon/label. The rawOutput metadata (child + parent session
        // ids) still marks the spawn — keep the naming.
        _ if update
            .get("rawOutput")
            .and_then(|r| r.get("metadata"))
            .is_some_and(|m| {
                m.get("sessionId").is_some() && m.get("parentSessionId").is_some()
            }) =>
        {
            ToolCall::Unknown {
                name: if title.is_empty() {
                    "Agent".into()
                } else {
                    format!("Agent: {title}")
                },
                input: raw.cloned(),
            }
        }
        _ if raw_str("_toolName").as_deref() == Some("task") => ToolCall::Unknown {
            name: raw_str("description")
                .filter(|d| d != "Subagent task")
                .map(|d| format!("Task: {d}"))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_else(|| {
                    if title.is_empty() {
                        "Task".into()
                    } else {
                        title
                    }
                }),
            input: raw.cloned(),
        },
        // OpenCode's todo tracker (kind "todowrite", or title like "2 todos"
        // with rawInput.todos of content/status/priority): normalize to a
        // real Todo chip instead of an Unknown JSON dump. Kept after the
        // task/agent arms so spawns win.
        _ if kind == "todowrite" || kind == "TodoWrite" || todo_items(raw).is_some() => {
            ToolCall::Todo {
                items: todo_items(raw).unwrap_or_default(),
            }
        }
        _ => ToolCall::Unknown {
            name: if title.is_empty() { kind.into() } else { title },
            input: raw.cloned(),
        },
    }
}

/// Todo items from a raw input carrying a `todos` array of
/// `{content, status}` (OpenCode todowrite/tracker, plan entries).
/// `Some` (possibly empty) when the shape matches, `None` otherwise.
fn todo_items(raw: Option<&serde_json::Value>) -> Option<Vec<TodoItem>> {
    let items = raw?.get("todos")?.as_array()?;
    if !items
        .iter()
        .all(|t| t.get("content").and_then(Value::as_str).is_some())
    {
        return None;
    }
    Some(
        items
            .iter()
            .map(|t| TodoItem {
                text: t
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                done: t.get("status").and_then(Value::as_str) == Some("completed"),
            })
            .collect(),
    )
}

fn u64_keys(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_u64))
}

/// Occupancy + window as computed by the agent itself.
///
/// Accepts ACP `usage_update` `{used,size}`, Grok `_x.ai/session/info`
/// `{context:{used,total}}`, and a few nested aliases. Billed last-turn
/// counters (`inputTokens` / `totalTokens`) are not occupancy — those
/// ride [`billed_usage_event`].
pub(crate) fn native_window(v: &Value) -> Option<(u64, u64)> {
    let nested = v.get("context").or_else(|| {
        v.get("usage").and_then(|usage| {
            (usage.get("used").is_some() || usage.get("size").is_some()).then_some(usage)
        })
    });
    let used = u64_keys(v, &["used", "tokensUsed", "tokens_used"])
        .or_else(|| nested.and_then(|c| u64_keys(c, &["used", "tokens", "tokensUsed"])));
    let size = u64_keys(
        v,
        &["size", "contextWindow", "context_window", "contextLimit"],
    )
    .or_else(|| {
        nested.and_then(|c| u64_keys(c, &["total", "size", "max", "limit", "contextWindow"]))
    });
    match (used, size) {
        (Some(used), Some(size)) if used > 0 || size > 0 => Some((used, size)),
        (Some(used), None) if used > 0 => Some((used, 0)),
        (None, Some(size)) if size > 0 => Some((0, size)),
        _ => None,
    }
}

pub(crate) fn native_context_event(v: &Value) -> Option<AgentEvent> {
    let (used, size) = native_window(v)?;
    Some(AgentEvent::ContextWindow {
        stats: ContextUsageStats::window(used, size, ContextUsageSource::Native, used, 0, 0, 0),
    })
}

/// Grok `_x.ai/session/info` is double-wrapped (`{result:{context:…}}`)
/// inside the JSON-RPC result; other agents return the object directly.
pub(crate) fn session_info_window(resp: &Value) -> Option<AgentEvent> {
    native_context_event(resp.get("result").unwrap_or(resp)).or_else(|| native_context_event(resp))
}

pub(crate) fn billed_usage_event(usage: &Value) -> Option<AgentEvent> {
    let input = u64_keys(usage, &["inputTokens", "input_tokens"]);
    let output = u64_keys(usage, &["outputTokens", "output_tokens"]);
    let cached = u64_keys(
        usage,
        &[
            "cachedReadTokens",
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cacheReadInputTokens",
        ],
    )
    .unwrap_or(0);
    let reasoning = u64_keys(usage, &["reasoningTokens", "reasoning_tokens"]).unwrap_or(0);
    (input.is_some() || output.is_some()).then(|| AgentEvent::Usage {
        input_tokens: input.unwrap_or(0),
        cached_input_tokens: cached,
        output_tokens: output.unwrap_or(0),
        reasoning_tokens: reasoning,
        context_limit: None,
    })
}

/// Map one `session/update` payload's `update` object to events.
/// Message/thought chunks are handled here too (unlike codex, ACP has no
/// separate delta channel).
pub(crate) fn map_update(update: &Value) -> Vec<AgentEvent> {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => chunk_text(update)
            .map(|text| vec![AgentEvent::TextDelta { text }])
            .unwrap_or_default(),
        "agent_thought_chunk" => chunk_text(update)
            .map(|text| vec![AgentEvent::ReasoningDelta { text }])
            .unwrap_or_default(),
        // Replayed history on session/load is filtered by the session loop
        // before this map; a live user chunk is our own prompt echoed back.
        "user_message_chunk" => Vec::new(),
        "tool_call" => {
            let id = str_field(update, "toolCallId");
            let mut events = vec![AgentEvent::ToolCall {
                id: id.clone(),
                call: typed_call(update),
            }];
            // Some agents send a single terminal-status `tool_call` with the
            // result inline instead of a follow-up update.
            if let Some(resolved) = resolved_result(update, id) {
                events.push(resolved);
            }
            events
        }
        "tool_call_update" => {
            let id = str_field(update, "toolCallId");
            let mut events = Vec::new();
            // Refresh the call only when the update carries new SHAPE — kind,
            // title, rawInput, or a diff. Result-only content (output text)
            // must not re-type the call: a kindless completion update would
            // clobber the opening call's `Exec` into `Unknown`.
            if update.get("kind").is_some()
                || update.get("title").is_some()
                || update.get("rawInput").is_some()
                || tool_diff(update).is_some()
            {
                events.push(AgentEvent::ToolCall {
                    id: id.clone(),
                    call: typed_call(update),
                });
            }
            if let Some(resolved) = resolved_result(update, id.clone()) {
                events.push(resolved);
            } else if let Some(text) = tool_output(update) {
                // Intermediate content: what the tool has printed so far.
                // ACP replaces a tool call's content array on every update,
                // so this is a snapshot, not a delta (append=false).
                events.push(AgentEvent::ToolProgress {
                    id,
                    text,
                    append: false,
                });
            }
            events
        }
        "plan" => {
            let items = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|e| TodoItem {
                    text: str_field(e, "content"),
                    done: e.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect();
            // The plan has no wire id; a stable synthetic id makes every
            // update refresh the same chip (fold refreshes in place by id).
            vec![
                AgentEvent::ToolCall {
                    id: komet_proto::LIVE_PLAN_TOOL_ID.into(),
                    call: ToolCall::Todo { items },
                },
                AgentEvent::ToolResult {
                    id: komet_proto::LIVE_PLAN_TOOL_ID.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                },
            ]
        }
        "available_commands_update" => {
            let commands = parse_commands(update.get("availableCommands"));
            vec![AgentEvent::AvailableCommands { commands }]
        }
        "usage_update" => native_context_event(update).into_iter().collect(),
        // Grok's accurate occupancy lives here (`context.{used,total}`),
        // not on billed `turn_completed.usage.inputTokens`.
        "session_info_update" => native_context_event(update).into_iter().collect(),
        "turn_completed" => {
            let mut events: Vec<_> = native_context_event(update).into_iter().collect();
            if let Some(usage) = billed_usage_event(update.get("usage").unwrap_or(update)) {
                events.push(usage);
            }
            events
        }
        // Mode/config updates carry nothing we render.
        "current_mode_update" | "config_option_update" => Vec::new(),
        // Draft ACP Session Compaction RFD — optional signals, and this client
        // only announces them to an agent validated in Gate B. Field spellings
        // are not settled, so every name below is read where it appears.
        "compaction_update" => {
            let id = str_field(update, "compactionId");
            if id.is_empty() {
                // Without an id there is no cycle to refine: the row would be
                // an orphan that nothing can ever settle.
                tracing::debug!("dropping an ACP compaction_update without a compactionId");
                return Vec::new();
            }
            vec![AgentEvent::ContextCompaction {
                id,
                // Absent or unknown status stays `inProgress`: the adapter does
                // not infer a lifecycle it was not told.
                status: draft_compaction_status(update.get("status")),
                trigger: opt_str(update, &["trigger", "reason"]),
                tokens_before: u64_keys(update, &["tokensBefore", "preTokens"]),
                tokens_after: u64_keys(update, &["tokensAfter", "postTokens"]),
                summary: opt_str(update, &["summary"]),
                summary_delta: None,
                error: opt_str(update, &["error", "message"]),
            }]
        }
        "compaction_summary_chunk" => {
            let id = str_field(update, "compactionId");
            if id.is_empty() {
                tracing::debug!("dropping an ACP compaction_summary_chunk without a compactionId");
                return Vec::new();
            }
            let Some(text) = opt_str(update, &["summaryDelta", "delta", "text"]) else {
                return Vec::new();
            };
            vec![AgentEvent::ContextCompaction {
                id,
                // A chunk says nothing about the cycle's state; `inProgress` is
                // the field's default, not a claim — and the engine never
                // reopens a settled cycle on it.
                status: komet_proto::CompactionStatus::default(),
                trigger: None,
                tokens_before: None,
                tokens_after: None,
                summary: None,
                summary_delta: Some(text),
                error: None,
            }]
        }
        _ => {
            // Unknown kinds still degrade to nothing, but not silently: a draft
            // signal this build does not map must show up in the log, or an
            // agent that started sending one reads as an agent that sends none.
            if !kind.is_empty() {
                tracing::debug!(update_kind = kind, "unmapped ACP sessionUpdate dropped");
            }
            Vec::new()
        }
    }
}

/// Status of a draft compaction signal. Snake and camel spellings both appear
/// in the RFD discussion; anything unrecognized (including absent) is
/// `inProgress`.
fn draft_compaction_status(value: Option<&Value>) -> komet_proto::CompactionStatus {
    use komet_proto::CompactionStatus;
    match value.and_then(Value::as_str).unwrap_or("") {
        "completed" | "complete" | "done" | "success" => CompactionStatus::Completed,
        "failed" | "error" | "errored" => CompactionStatus::Failed,
        "cancelled" | "canceled" => CompactionStatus::Cancelled,
        _ => CompactionStatus::InProgress,
    }
}

/// First non-blank string under any of `keys`.
fn opt_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| v.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())
}

/// A terminal `status` on a tool_call/tool_call_update resolves the call:
/// `completed`/`failed` → ToolResult with capped output + inline diff.
fn resolved_result(update: &Value, id: String) -> Option<AgentEvent> {
    let status = update.get("status").and_then(Value::as_str)?;
    let is_error = match status {
        "completed" => false,
        "failed" => true,
        _ => return None,
    };
    Some(AgentEvent::ToolResult {
        id,
        is_error,
        output: tool_output(update),
        diff: tool_diff(update),
    })
}

/// Decode an `availableCommands` array (`{name, description, input: {hint}}`).
pub(crate) fn parse_commands(value: Option<&Value>) -> Vec<SlashCommand> {
    value
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let name = str_field(c, "name");
            (!name.is_empty()).then(|| SlashCommand {
                name,
                description: str_field(c, "description"),
                input_hint: c
                    .get("input")
                    .and_then(|i| i.get("hint"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                ..SlashCommand::default()
            })
        })
        .collect()
}

/// `session/request_permission` options (`{optionId, name, kind}`) → the
/// preferred auto-approve choice: `allow_always` > `allow_once` > first.
pub(crate) fn preferred_allow_option(options: &[Value]) -> Option<String> {
    let by_kind = |kind: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
    };
    by_kind("allow_always")
        .or_else(|| by_kind("allow_once"))
        .or_else(|| options.first())
        .map(|o| str_field(o, "optionId"))
        .filter(|id| !id.is_empty())
}

/// Byte cap for one free-text elicitation answer, before it reaches the agent
/// and the doc (the same 4 KiB bound as tool output).
pub(crate) const ELICITATION_TEXT_CAP: usize = 4096;

/// What a mapped elicitation field expects back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElicitationKind {
    /// `string` — free text, constraints in the question.
    Text,
    /// `number` — free text answered back as a JSON number.
    Number,
    /// `integer` — free text answered back as a JSON integer.
    Integer,
    /// `boolean` — rendered Yes/No, answered as a bool.
    Boolean,
    /// `enum`/`oneOf`/`anyOf` — exactly one label.
    Single,
    /// `array` of an enum — several labels.
    Multi,
}

/// One property of an elicitation form, mapped to one question.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ElicitationField {
    /// Schema property name; doubles as the question id so the answer maps
    /// back without a side table.
    pub(crate) name: String,
    pub(crate) required: bool,
    pub(crate) kind: ElicitationKind,
    /// Option label → the value the agent receives. Empty for Text/Boolean.
    pub(crate) choices: Vec<(String, Value)>,
    pub(crate) question: UserInputQuestion,
}

/// A mapped `elicitation/create` form, or why it cannot be rendered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ElicitationForm {
    Ask {
        fields: Vec<ElicitationField>,
    },
    /// Never approximated into a form: the caller refuses explicitly.
    Unsupported(String),
}

/// Map an ACP draft `elicitation/create` request (`mode: "form"`) to
/// questions. Draft RFD, no agent verified emitting it (Gate B) — the schema
/// subset follows the RFD: an object of scalar/enum properties, `required[]
/// naming required ones, optional `mode` (absent = form).
///
/// Anything outside that subset (url mode, nested objects, arrays of
/// non-enums, empty or typeless properties) is [`ElicitationForm::Unsupported`]
/// with a human reason — a guessed form would ask the user something the agent
/// never asked. Properties come in key order (serde_json's map is sorted), so
/// the wizard's pagination is deterministic but not necessarily the agent's
/// declaration order.
pub(crate) fn elicitation_form(params: &Value) -> ElicitationForm {
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("form");
    if mode != "form" {
        return ElicitationForm::Unsupported(format!(
            "elicitation mode `{mode}` is not supported (form only)"
        ));
    }
    let Some(schema) = params.get("requestedSchema").and_then(Value::as_object) else {
        return ElicitationForm::Unsupported("the request carries no requestedSchema".into());
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return ElicitationForm::Unsupported("the requestedSchema is not an object".into());
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return ElicitationForm::Unsupported("the requestedSchema has no properties".into());
    };
    if properties.is_empty() {
        return ElicitationForm::Unsupported("the requestedSchema asks nothing".into());
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut fields = Vec::with_capacity(properties.len());
    for (name, prop) in properties {
        match elicitation_field(name, prop, required.contains(&name.as_str()), message) {
            Ok(field) => fields.push(field),
            Err(reason) => {
                return ElicitationForm::Unsupported(format!("property `{name}`: {reason}"));
            }
        }
    }
    ElicitationForm::Ask { fields }
}

fn elicitation_field(
    name: &str,
    prop: &Value,
    required: bool,
    message: &str,
) -> Result<ElicitationField, String> {
    let title = prop
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or(name);
    let description = prop
        .get("description")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty());
    let mut question = description
        .or((!message.is_empty()).then_some(message))
        .unwrap_or(title)
        .to_owned();

    let (kind, choices, descriptions) = if prop.get("type").and_then(Value::as_str) == Some("array")
    {
        let items = prop.get("items").unwrap_or(&Value::Null);
        let values = items.get("enum").and_then(Value::as_array);
        let variants = items
            .get("oneOf")
            .or_else(|| items.get("anyOf"))
            .and_then(Value::as_array);
        if let Some(values) = values.filter(|v| !v.is_empty()) {
            (
                ElicitationKind::Multi,
                enum_choices(values, items.get("enumNames")),
                Vec::new(),
            )
        } else if let Some(variants) = variants.filter(|v| !v.is_empty()) {
            let (choices, descriptions) = variant_choices(variants)?;
            (ElicitationKind::Multi, choices, descriptions)
        } else {
            return Err("array without an enum of items".into());
        }
    } else if let Some(values) = prop.get("enum").and_then(Value::as_array) {
        if values.is_empty() {
            return Err("empty enum".into());
        }
        (
            ElicitationKind::Single,
            enum_choices(values, prop.get("enumNames")),
            Vec::new(),
        )
    } else if let Some(variants) = prop
        .get("oneOf")
        .or_else(|| prop.get("anyOf"))
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
    {
        let (choices, descriptions) = variant_choices(variants)?;
        (ElicitationKind::Single, choices, descriptions)
    } else {
        match prop.get("type").and_then(Value::as_str) {
            Some("string" | "integer" | "number") => {
                if let Some(hint) = constraint_hint(prop) {
                    question = format!("{question}\n({hint})");
                }
                let kind = match prop.get("type").and_then(Value::as_str) {
                    Some("number") => ElicitationKind::Number,
                    Some("integer") => ElicitationKind::Integer,
                    _ => ElicitationKind::Text,
                };
                (kind, Vec::new(), Vec::new())
            }
            Some("boolean") => (
                ElicitationKind::Boolean,
                vec![
                    ("Yes".to_owned(), Value::Bool(true)),
                    ("No".to_owned(), Value::Bool(false)),
                ],
                Vec::new(),
            ),
            Some(other) => return Err(format!("unsupported property type `{other}`")),
            None => return Err("property without a type".into()),
        }
    };

    let default = match kind {
        ElicitationKind::Single => prop.get("default").and_then(|default| {
            choices
                .iter()
                .find(|(_, value)| value == default)
                .map(|(label, _)| label.clone())
        }),
        ElicitationKind::Boolean => prop
            .get("default")
            .and_then(Value::as_bool)
            .map(|b| if b { "Yes" } else { "No" }.to_owned()),
        // A multi-select default is an array; `default` carries one label.
        ElicitationKind::Multi => None,
        ElicitationKind::Text | ElicitationKind::Number | ElicitationKind::Integer => {
            prop.get("default").map(|default| match default {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
        }
    };
    let options: Vec<String> = choices.iter().map(|(label, _)| label.clone()).collect();
    Ok(ElicitationField {
        name: name.to_owned(),
        required,
        kind,
        choices,
        question: UserInputQuestion {
            id: name.to_owned(),
            header: title.to_owned(),
            question,
            options,
            multi_select: kind == ElicitationKind::Multi,
            option_descriptions: descriptions,
            // Free text only where the schema actually allows any value.
            allow_free_text: matches!(
                kind,
                ElicitationKind::Text | ElicitationKind::Number | ElicitationKind::Integer
            ),
            secret: prop.get("format").and_then(Value::as_str) == Some("password"),
            default,
        },
    })
}

/// `enum` + optional `enumNames` (aligned labels) → label/value pairs.
fn enum_choices(values: &[Value], names: Option<&Value>) -> Vec<(String, Value)> {
    let names: Vec<&str> = names
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    values
        .iter()
        .enumerate()
        .map(|(ix, value)| {
            let label = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let label = names
                .get(ix)
                .filter(|name| !name.is_empty())
                .map(|name| (*name).to_owned())
                .unwrap_or(label);
            (label, value.clone())
        })
        .collect()
}

/// `oneOf`/`anyOf` `{const, title, description}` variants → label/value pairs
/// plus descriptions aligned with them (empty only when all are empty).
fn variant_choices(variants: &[Value]) -> Result<(Vec<(String, Value)>, Vec<String>), String> {
    let mut choices = Vec::with_capacity(variants.len());
    let mut descriptions = Vec::with_capacity(variants.len());
    for variant in variants {
        let Some(value) = variant.get("const").cloned() else {
            return Err("oneOf/anyOf entry without const".into());
        };
        let label = variant
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| match &value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
        descriptions.push(
            variant
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        );
        choices.push((label, value));
    }
    if descriptions.iter().all(String::is_empty) {
        descriptions.clear();
    }
    Ok((choices, descriptions))
}

/// A one-line hint for a text field's constraints, so the user sees the rule
/// rather than discovering it in the agent's rejection.
fn constraint_hint(prop: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(pattern) = prop.get("pattern").and_then(Value::as_str) {
        parts.push(format!("must match `{pattern}`"));
    }
    if let Some(min) = prop.get("minLength").or_else(|| prop.get("minimum")) {
        parts.push(format!("min {min}"));
    }
    if let Some(max) = prop.get("maxLength").or_else(|| prop.get("maximum")) {
        parts.push(format!("max {max}"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Build the `elicitation/create` response from the user's answers.
///
/// The three outcomes are distinct: no answers at all is `cancel` (the user
/// dismissed the form), a missing required field is `decline`, and only a form
/// with values is `accept`. There is no path to `accept` with invented content
/// — a dropped resolver reads as `cancel`, never as consent.
pub(crate) fn elicitation_response(form: &ElicitationForm, answers: &[UserInputAnswer]) -> Value {
    let fields = match form {
        ElicitationForm::Ask { fields } => fields,
        ElicitationForm::Unsupported(_) => return serde_json::json!({ "action": "decline" }),
    };
    if answers.is_empty() {
        return serde_json::json!({ "action": "cancel" });
    }
    let mut content = serde_json::Map::new();
    for field in fields {
        let answer = answers.iter().find(|a| a.question_id == field.name);
        match field_value(field, answer) {
            Some((name, value)) => {
                content.insert(name, value);
            }
            None if field.required => return serde_json::json!({ "action": "decline" }),
            None => {}
        }
    }
    if content.is_empty() {
        return serde_json::json!({ "action": "decline" });
    }
    serde_json::json!({ "action": "accept", "content": Value::Object(content) })
}

fn field_value(
    field: &ElicitationField,
    answer: Option<&UserInputAnswer>,
) -> Option<(String, Value)> {
    let answer = answer?;
    let typed = answer
        .free_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let label = || typed.or_else(|| answer.labels.first().map(String::as_str));
    let value = match field.kind {
        ElicitationKind::Text => {
            let text = label()?;
            Value::String(cap_text(text, ELICITATION_TEXT_CAP))
        }
        ElicitationKind::Number => {
            let text = label()?;
            let number = text
                .parse::<i64>()
                .map(serde_json::Number::from)
                .ok()
                .or_else(|| {
                    text.parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                })?;
            Value::Number(number)
        }
        ElicitationKind::Integer => {
            let text = label()?;
            Value::Number(text.parse::<i64>().ok()?.into())
        }
        ElicitationKind::Boolean => {
            let text = label()?;
            if text.eq_ignore_ascii_case("yes") || text.eq_ignore_ascii_case("true") {
                Value::Bool(true)
            } else if text.eq_ignore_ascii_case("no") || text.eq_ignore_ascii_case("false") {
                Value::Bool(false)
            } else {
                return None;
            }
        }
        ElicitationKind::Single => {
            let text = label()?;
            field
                .choices
                .iter()
                .find(|(choice, _)| choice == text)
                .map(|(_, value)| value.clone())?
        }
        ElicitationKind::Multi => {
            let picked: Vec<Value> = match typed {
                // Free text on a closed list only matches an exact label.
                Some(text) => field
                    .choices
                    .iter()
                    .filter(|(choice, _)| choice == text)
                    .map(|(_, value)| value.clone())
                    .collect(),
                None => field
                    .choices
                    .iter()
                    .filter(|(choice, _)| answer.labels.iter().any(|label| label == choice))
                    .map(|(_, value)| value.clone())
                    .collect(),
            };
            if picked.is_empty() {
                return None;
            }
            Value::Array(picked)
        }
    };
    Some((field.name.clone(), value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn acp_intermediate_tool_content_emits_progress() {
        // Non-terminal content is a SNAPSHOT (ACP replaces the content array),
        // so it maps to ToolProgress with append=false, not to a ToolResult.
        let events = map_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "in_progress",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "compiling…"}}
            ]
        }));
        assert_eq!(
            events,
            vec![AgentEvent::ToolProgress {
                id: "c1".into(),
                text: "compiling…".into(),
                append: false,
            }]
        );
        // A terminal update still resolves as a result — never progress.
        let done = map_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "ok"}}
            ]
        }));
        assert!(
            done.iter()
                .all(|e| !matches!(e, AgentEvent::ToolProgress { .. })),
            "{done:?}"
        );
        assert!(matches!(
            done.last(),
            Some(AgentEvent::ToolResult { output: Some(o), .. }) if o == "ok"
        ));
    }

    #[test]
    fn opencode_todo_tracker_maps_to_todo_chip() {
        // Shape seen live: title "2 todos", rawInput.todos with
        // content/status/priority — must not fall to Unknown JSON dump.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t-todo",
            "status": "completed",
            "kind": "other",
            "title": "2 todos",
            "rawInput": {
                "todos": [
                    { "content": "a", "status": "completed", "priority": "high" },
                    { "content": "b", "status": "in_progress", "priority": "high" },
                ],
            },
        });
        let events = map_update(&update);
        assert!(matches!(
            &events[0],
            AgentEvent::ToolCall {
                call: ToolCall::Todo { items },
                ..
            } if items == &vec![
                TodoItem { text: "a".into(), done: true },
                TodoItem { text: "b".into(), done: false },
            ]
        ));
    }

    #[test]
    fn todowrite_kind_without_raw_maps_to_empty_todo() {
        // Opening update: kind only, no rawInput yet — Todo, not Unknown.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t-todo",
            "kind": "todowrite",
        });
        let events = map_update(&update);
        assert!(matches!(
            &events[0],
            AgentEvent::ToolCall {
                call: ToolCall::Todo { items },
                ..
            } if items.is_empty()
        ));
    }

    #[test]
    fn message_and_thought_chunks_map_to_deltas() {
        let update = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::TextDelta {
                text: "hello".into()
            }]
        );
        let thought = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" },
        });
        assert_eq!(
            map_update(&thought),
            vec![AgentEvent::ReasoningDelta { text: "hmm".into() }]
        );
        // Non-text blocks render as nothing.
        let image = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "image", "data": "...", "mimeType": "image/png" },
        });
        assert_eq!(map_update(&image), Vec::new());
    }

    #[test]
    fn execute_tool_call_with_terminal_status_resolves_inline() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "ls -la",
            "kind": "execute",
            "status": "completed",
            "rawInput": { "command": "ls -la" },
            "content": [
                { "type": "content", "content": { "type": "text", "text": "total 0" } },
            ],
        });
        assert_eq!(
            map_update(&update),
            vec![
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ToolCall::Exec {
                        command: "ls -la".into()
                    },
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: false,
                    output: Some("total 0".into()),
                    diff: None,
                },
            ]
        );
    }

    #[test]
    fn edit_with_diff_content_maps_to_edit_file_and_carries_diff() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t2",
            "kind": "edit",
            "status": "completed",
            "content": [{
                "type": "diff",
                "path": "/w/src/main.rs",
                "oldText": "fn old() {}",
                "newText": "fn new() {}",
            }],
        });
        let events = map_update(&update);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                id: "t2".into(),
                call: ToolCall::EditFile {
                    path: "/w/src/main.rs".into(),
                    old_string: None,
                    new_string: None,
                },
            }
        );
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                id: "t2".into(),
                is_error: false,
                output: None,
                diff: Some(ToolDiff {
                    path: "/w/src/main.rs".into(),
                    old_text: Some("fn old() {}".into()),
                    new_text: "fn new() {}".into(),
                }),
            }
        );
    }

    #[test]
    fn new_file_diff_maps_to_write_file() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "kind": "edit",
            "content": [{ "type": "diff", "path": "/w/new.rs", "newText": "x" }],
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t3".into(),
                call: ToolCall::WriteFile {
                    path: "/w/new.rs".into(),
                    content: None
                },
            }]
        );
    }

    #[test]
    fn status_only_update_resolves_without_refreshing_call() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t4",
            "status": "failed",
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolResult {
                id: "t4".into(),
                is_error: true,
                output: None,
                diff: None,
            }]
        );
    }

    #[test]
    fn plan_maps_to_stable_todo_chip() {
        let update = json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "read code", "priority": "high", "status": "completed" },
                { "content": "write fix", "priority": "high", "status": "in_progress" },
            ],
        });
        let events = map_update(&update);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                id: komet_proto::LIVE_PLAN_TOOL_ID.into(),
                call: ToolCall::Todo {
                    items: vec![
                        TodoItem {
                            text: "read code".into(),
                            done: true
                        },
                        TodoItem {
                            text: "write fix".into(),
                            done: false
                        },
                    ]
                },
            }
        );
    }

    #[test]
    fn available_commands_parse_with_hint() {
        let update = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "compact", "description": "Compact the session" },
                { "name": "goal", "description": "Set a goal", "input": { "hint": "the goal" } },
                { "description": "nameless is dropped" },
            ],
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::AvailableCommands {
                commands: vec![
                    SlashCommand {
                        name: "compact".into(),
                        description: "Compact the session".into(),
                        input_hint: None,
                        ..SlashCommand::default()
                    },
                    SlashCommand {
                        name: "goal".into(),
                        description: "Set a goal".into(),
                        input_hint: Some("the goal".into()),
                        ..SlashCommand::default()
                    },
                ]
            }]
        );
    }

    #[test]
    fn usage_update_maps_to_context_window() {
        let events = map_update(&json!({
            "sessionUpdate": "usage_update",
            "used": 1200,
            "size": 500000,
        }));
        match &events[..] {
            [AgentEvent::ContextWindow { stats }] => {
                assert_eq!(stats.used(), 1200);
                assert_eq!(stats.context_limit, 500_000);
                assert_eq!(stats.source, komet_proto::ContextUsageSource::Native);
            }
            other => panic!("expected ContextWindow, got {other:?}"),
        }
        assert!(map_update(&json!({"sessionUpdate": "current_mode_update"})).is_empty());
    }

    /// The draft Session Compaction RFD as one cycle: an opening notice, chunks
    /// that only add recap text, and a terminal status. The normalizer maps each
    /// frame as it arrives — accumulating the chunks is the doc fold's job, not
    /// the adapter's.
    #[test]
    fn acp_compaction_update_maps_lifecycle_and_chunks() {
        let opened = map_update(&json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "cmp-7",
            "status": "in_progress",
        }));
        assert!(
            matches!(
                &opened[..],
                [AgentEvent::ContextCompaction {
                    id,
                    status: komet_proto::CompactionStatus::InProgress,
                    summary: None,
                    ..
                }] if id == "cmp-7"
            ),
            "{opened:?}"
        );

        let deltas: Vec<_> = ["First part. ", "second part."]
            .iter()
            .flat_map(|text| {
                map_update(&json!({
                    "sessionUpdate": "compaction_summary_chunk",
                    "compactionId": "cmp-7",
                    "summaryDelta": text,
                }))
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        for (event, text) in deltas.iter().zip(["First part. ", "second part."]) {
            assert!(
                matches!(
                    event,
                    AgentEvent::ContextCompaction {
                        id,
                        summary_delta: Some(delta),
                        summary: None,
                        ..
                    } if id == "cmp-7" && delta == text
                ),
                "{event:?}"
            );
        }

        let closed = map_update(&json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "cmp-7",
            "status": "completed",
            "trigger": "auto",
            "tokensBefore": 165_000,
            "summary": "the whole recap",
        }));
        assert!(
            matches!(
                &closed[..],
                [AgentEvent::ContextCompaction {
                    id,
                    status: komet_proto::CompactionStatus::Completed,
                    trigger: Some(trigger),
                    tokens_before: Some(165_000),
                    // The RFD carries no post-compaction size: stays unknown.
                    tokens_after: None,
                    summary: Some(summary),
                    ..
                }] if id == "cmp-7" && trigger == "auto" && summary == "the whole recap"
            ),
            "{closed:?}"
        );
    }

    /// An unannounced status is not a completed cycle, and a chunk that arrives
    /// alone does not claim one either.
    #[test]
    fn compaction_status_is_never_inferred() {
        let events =
            map_update(&json!({"sessionUpdate": "compaction_update", "compactionId": "c"}));
        assert!(
            matches!(
                &events[..],
                [AgentEvent::ContextCompaction {
                    status: komet_proto::CompactionStatus::InProgress,
                    ..
                }]
            ),
            "{events:?}"
        );
        let events = map_update(&json!({
            "sessionUpdate": "compaction_summary_chunk",
            "compactionId": "c",
            "summaryDelta": "x",
        }));
        assert!(
            matches!(
                &events[..],
                [AgentEvent::ContextCompaction {
                    status: komet_proto::CompactionStatus::InProgress,
                    summary_delta: Some(_),
                    ..
                }]
            ),
            "{events:?}"
        );
        for status in ["failed", "cancelled"] {
            let events = map_update(&json!({
                "sessionUpdate": "compaction_update",
                "compactionId": "c",
                "status": status,
                "error": "summarization timed out",
            }));
            assert!(
                matches!(
                    &events[..],
                    [AgentEvent::ContextCompaction {
                        status: komet_proto::CompactionStatus::Failed
                            | komet_proto::CompactionStatus::Cancelled,
                        error: Some(message),
                        ..
                    }] if message == "summarization timed out"
                ),
                "{status}: {events:?}"
            );
        }
    }

    /// A compaction signal with no id cannot refine anything: it is dropped, and
    /// so is every unmapped kind — each through a categorized debug log rather
    /// than a silent `continue` (transparency plan, Task 12 Step 1).
    #[test]
    fn unknown_update_kind_is_logged_and_dropped() {
        assert!(
            map_update(&json!({"sessionUpdate": "compaction_update", "status": "completed"}))
                .is_empty(),
            "a cycle without an id has nothing to refine"
        );
        for update in [
            json!({"sessionUpdate": "something_from_a_future_draft", "value": 1}),
            json!({"sessionUpdate": "compaction_summary_chunk"}),
            json!({
                "sessionUpdate": "compaction_summary_chunk",
                "summaryDelta": "no id to settle",
            }),
            json!({"sessionUpdate": ""}),
        ] {
            assert!(map_update(&update).is_empty(), "{update:?}");
        }
    }

    #[test]
    fn grok_session_info_is_native_occupancy_not_billed_tokens() {
        let events = map_update(&json!({
            "sessionUpdate": "session_info_update",
            "context": { "used": 42_000, "total": 131_072, "systemPromptTokens": 1_200 },
        }));
        match &events[..] {
            [AgentEvent::ContextWindow { stats }] => {
                assert_eq!(stats.used(), 42_000);
                assert_eq!(stats.context_limit, 131_072);
                assert_eq!(stats.source, komet_proto::ContextUsageSource::Native);
            }
            other => panic!("expected ContextWindow, got {other:?}"),
        }

        let wrapped = json!({
            "result": { "turns": 3, "context": { "used": 18_000, "total": 131_072 } }
        });
        match session_info_window(&wrapped) {
            Some(AgentEvent::ContextWindow { stats }) => {
                assert_eq!(stats.used(), 18_000);
                assert_eq!(stats.context_limit, 131_072);
            }
            other => panic!("expected session/info window, got {other:?}"),
        }
    }

    #[test]
    fn turn_completed_billed_usage_is_not_a_window() {
        let events = map_update(&json!({
            "sessionUpdate": "turn_completed",
            "usage": {
                "inputTokens": 3_000_000,
                "outputTokens": 18_000,
                "totalTokens": 3_018_000,
                "cachedReadTokens": 5_376,
            },
        }));
        match &events[..] {
            [
                AgentEvent::Usage {
                    input_tokens: 3_000_000,
                    cached_input_tokens: 5_376,
                    output_tokens: 18_000,
                    ..
                },
            ] => {}
            other => panic!("expected billed Usage only, got {other:?}"),
        }

        let with_context = map_update(&json!({
            "sessionUpdate": "turn_completed",
            "context": { "used": 24_000, "total": 131_072 },
            "usage": { "inputTokens": 3_000_000, "outputTokens": 36 },
        }));
        assert!(with_context.iter().any(|e| matches!(
            e,
            AgentEvent::ContextWindow { stats }
                if stats.used() == 24_000 && stats.context_limit == 131_072
        )));
        assert!(with_context.iter().any(|e| matches!(
            e,
            AgentEvent::Usage {
                input_tokens: 3_000_000,
                output_tokens: 36,
                ..
            }
        )));
    }

    #[test]
    fn usage_update_overshoot_is_capped_to_the_window() {
        let events = map_update(&json!({
            "sessionUpdate": "usage_update",
            "used": 3_000_000,
            "size": 128_000,
        }));
        match &events[..] {
            [AgentEvent::ContextWindow { stats }] => {
                assert_eq!(stats.used(), 128_000);
                assert_eq!(stats.context_limit, 128_000);
                assert_eq!(stats.input_tokens, 3_000_000);
                assert_eq!(stats.source, komet_proto::ContextUsageSource::Native);
            }
            other => panic!("expected ContextWindow, got {other:?}"),
        }
    }

    #[test]
    fn output_and_diff_caps_apply() {
        let big = "x".repeat(OUTPUT_CAP + 100);
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t5",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "text", "text": big } },
            ],
        });
        let events = map_update(&update);
        // The content-bearing update refreshes the call, then resolves it.
        let Some(AgentEvent::ToolResult {
            output: Some(output),
            ..
        }) = events.last()
        else {
            panic!("expected resolved result, got {events:?}");
        };
        assert!(output.len() < OUTPUT_CAP + 32);
        assert!(output.ends_with("… [truncated]"));
    }

    #[test]
    fn permission_options_prefer_allow_always() {
        let options = vec![
            json!({ "optionId": "once", "name": "Allow once", "kind": "allow_once" }),
            json!({ "optionId": "always", "name": "Always", "kind": "allow_always" }),
            json!({ "optionId": "no", "name": "Reject", "kind": "reject_once" }),
        ];
        assert_eq!(preferred_allow_option(&options), Some("always".into()));
        let only_reject = vec![json!({ "optionId": "no", "kind": "reject_once" })];
        assert_eq!(preferred_allow_option(&only_reject), Some("no".into()));
        assert_eq!(preferred_allow_option(&[]), None);
    }

    /// Cursor ACP opens tool cards with a display `title` before `rawInput`
    /// is filled (Search "grep", Read "Read File", Web "Web Fetch"). Those
    /// labels must not become typed args.
    #[test]
    fn cursor_placeholder_titles_are_not_typed_args() {
        let grep = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "grep",
            "kind": "search",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&grep),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Search {
                    pattern: String::new(),
                    path: None,
                },
            }]
        );

        let read = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t2",
            "title": "Read File",
            "kind": "read",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&read),
            vec![AgentEvent::ToolCall {
                id: "t2".into(),
                call: ToolCall::ReadFile {
                    path: String::new(),
                },
            }]
        );

        let fetch = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "title": "Web Fetch",
            "kind": "fetch",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&fetch),
            vec![AgentEvent::ToolCall {
                id: "t3".into(),
                call: ToolCall::WebSearch {
                    query: String::new(),
                },
            }]
        );

        let web = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t4",
            "title": "Web Search",
            "kind": "search",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&web),
            vec![AgentEvent::ToolCall {
                id: "t4".into(),
                call: ToolCall::Search {
                    pattern: String::new(),
                    path: None,
                },
            }]
        );
    }

    #[test]
    fn cursor_search_term_maps_to_web_search() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Web Search: \"multitask cli\"",
            "kind": "search",
            "rawInput": { "searchTerm": "multitask cli" },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::WebSearch {
                    query: "multitask cli".into(),
                },
            }]
        );
    }

    #[test]
    fn cursor_exec_title_unwraps_markdown_backtick_escapes() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "`echo \\`hi\\``",
            "kind": "execute",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "echo `hi`".into(),
                },
            }]
        );
    }

    #[test]
    fn cursor_task_uses_description_not_placeholder_title() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Task: Subagent task",
            "kind": "other",
            "rawInput": {
                "_toolName": "task",
                "description": "Look up multitask docs",
                "prompt": "find the reminder",
            },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Unknown {
                    name: "Task: Look up multitask docs".into(),
                    input: Some(json!({
                        "_toolName": "task",
                        "description": "Look up multitask docs",
                        "prompt": "find the reminder",
                    })),
                },
            }]
        );
    }

    #[test]
    fn opencode_task_keeps_agent_naming_across_frames() {
        // The rawInput frame (in_progress) names the chip off the spawn args.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "in_progress",
            "kind": "think",
            "title": "Viz probe",
            "rawInput": {
                "description": "Viz probe",
                "prompt": "run the probe",
                "subagent_type": "general",
            },
        });
        assert!(matches!(
            map_update(&update).as_slice(),
            [AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. }]
                if name == "Agent: Viz probe"
        ));
        // The completion drops rawInput (title = the bare description); the
        // rawOutput metadata still marks the spawn — naming survives.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "title": "Viz probe",
            "content": [{"type": "content", "content": {"type": "text",
                "text": "<task id=\"ses_c\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>"}}],
            "rawOutput": {
                "output": "<task id=\"ses_c\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>",
                "metadata": {"parentSessionId": "ses_p", "sessionId": "ses_c"},
            },
        });
        assert!(matches!(
            map_update(&update).as_slice(),
            [
                AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. },
                AgentEvent::ToolResult { is_error: false, .. },
            ] if name == "Agent: Viz probe"
        ));
    }

    fn answer(question: &str, labels: &[&str]) -> UserInputAnswer {
        UserInputAnswer {
            question_id: question.into(),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            free_text: None,
        }
    }

    fn typed(question: &str, text: &str) -> UserInputAnswer {
        UserInputAnswer {
            question_id: question.into(),
            labels: vec![text.to_owned()],
            free_text: Some(text.to_owned()),
        }
    }

    fn fields(form: &ElicitationForm) -> &[ElicitationField] {
        match form {
            ElicitationForm::Ask { fields } => fields,
            ElicitationForm::Unsupported(reason) => panic!("unexpected refusal: {reason}"),
        }
    }

    #[test]
    fn elicitation_single_select_maps_to_one_question() {
        let params = json!({
            "message": "Which environment should I deploy to?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "env": {
                        "type": "string",
                        "title": "Environment",
                        "description": "Pick the target",
                        "enum": ["staging", "prod"],
                        "enumNames": ["Staging", "Production"],
                        "default": "staging",
                    },
                },
                "required": ["env"],
            },
        });
        let form = elicitation_form(&params);
        let fields = fields(&form);
        assert_eq!(fields.len(), 1);
        let field = &fields[0];
        assert_eq!(field.name, "env");
        assert!(field.required);
        assert_eq!(field.kind, ElicitationKind::Single);
        assert_eq!(
            field.question.id, "env",
            "the property name doubles as the question id"
        );
        assert_eq!(field.question.header, "Environment");
        assert_eq!(field.question.question, "Pick the target");
        assert_eq!(field.question.options, ["Staging", "Production"]);
        assert_eq!(field.question.default.as_deref(), Some("Staging"));
        assert!(!field.question.allow_free_text && !field.question.multi_select);
        assert_eq!(
            elicitation_response(&form, &[answer("env", &["Production"])]),
            json!({ "action": "accept", "content": { "env": "prod" } }),
            "the label maps back to the schema value, not its display name"
        );
    }

    #[test]
    fn elicitation_one_of_carries_descriptions() {
        let params = json!({
            "message": "How should the change land?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "strategy": {
                        "oneOf": [
                            { "const": "squash", "title": "Squash", "description": "One commit" },
                            { "const": "rebase", "title": "Rebase", "description": "Keep history" },
                        ],
                    },
                },
            },
        });
        let form = elicitation_form(&params);
        let field = &fields(&form)[0];
        assert_eq!(field.kind, ElicitationKind::Single);
        assert_eq!(field.choices.len(), 2);
        assert_eq!(
            field.question.option_descriptions,
            ["One commit", "Keep history"]
        );
        assert_eq!(
            elicitation_response(&form, &[answer("strategy", &["Rebase"])]),
            json!({ "action": "accept", "content": { "strategy": "rebase" } })
        );
    }

    #[test]
    fn elicitation_multi_select_array_maps_labels_back() {
        let params = json!({
            "message": "Which checks should run?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "checks": {
                        "type": "array",
                        "title": "Checks",
                        "items": {
                            "type": "string",
                            "enum": ["clippy", "fmt", "test"],
                            "enumNames": ["Clippy", "Format", "Tests"],
                        },
                    },
                },
                "required": ["checks"],
            },
        });
        let form = elicitation_form(&params);
        let field = &fields(&form)[0];
        assert_eq!(field.kind, ElicitationKind::Multi);
        assert!(field.question.multi_select);
        assert_eq!(field.question.options, ["Clippy", "Format", "Tests"]);
        assert_eq!(
            elicitation_response(&form, &[answer("checks", &["Tests", "Clippy"])]),
            json!({
                "action": "accept",
                "content": { "checks": ["clippy", "test"] },
            }),
            "picked labels come back in enum order as wire values"
        );
    }

    #[test]
    fn elicitation_boolean_and_text_fields_keep_their_shape() {
        let params = json!({
            "message": "Confirm the release",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "confirm": { "type": "boolean", "title": "Confirm", "default": true },
                    "tag": {
                        "type": "string",
                        "title": "Tag",
                        "pattern": "^v[0-9]+\\.[0-9]+$",
                        "minLength": 3,
                        "maxLength": 20,
                        "default": "v1.0",
                    },
                    "token": { "type": "string", "format": "password", "title": "Token" },
                    "retries": { "type": "integer", "minimum": 1, "maximum": 5 },
                },
                "required": ["confirm", "tag"],
            },
        });
        let form = elicitation_form(&params);
        let fields = fields(&form);
        // serde_json's default map is a BTreeMap, so properties arrive in key
        // order — the assertion below is by name for that reason.
        let by_name = |name: &str| {
            fields
                .iter()
                .find(|field| field.name == name)
                .unwrap_or_else(|| panic!("no field `{name}`"))
        };
        let confirm = by_name("confirm");
        assert_eq!(confirm.kind, ElicitationKind::Boolean);
        assert_eq!(confirm.question.options, ["Yes", "No"]);
        assert_eq!(confirm.question.default.as_deref(), Some("Yes"));
        let tag = by_name("tag");
        assert_eq!(tag.kind, ElicitationKind::Text);
        assert!(tag.question.allow_free_text);
        assert!(
            tag.question
                .question
                .contains("must match `^v[0-9]+\\.[0-9]+$`")
                && tag.question.question.contains("min 3")
                && tag.question.question.contains("max 20"),
            "{}",
            tag.question.question
        );
        assert_eq!(tag.question.default.as_deref(), Some("v1.0"));
        let token = by_name("token");
        assert!(token.question.secret, "format:password is a masked field");
        let retries = by_name("retries");
        assert_eq!(retries.kind, ElicitationKind::Integer);
        assert!(!confirm.question.allow_free_text && !confirm.question.multi_select);
        assert_eq!(
            elicitation_response(
                &form,
                &[
                    answer("confirm", &["No"]),
                    answer("tag", &["v2.1"]),
                    typed("retries", "3"),
                ],
            ),
            json!({
                "action": "accept",
                "content": { "confirm": false, "tag": "v2.1", "retries": 3 },
            }),
            "an integer field goes back as a number, not the typed string"
        );
    }

    #[test]
    fn elicitation_refuses_what_it_cannot_render() {
        let url = json!({ "mode": "url", "message": "Sign in", "requestedSchema": {} });
        assert!(matches!(
            elicitation_form(&url),
            ElicitationForm::Unsupported(reason) if reason.contains("url")
        ));
        let nested = json!({
            "requestedSchema": {
                "type": "object",
                "properties": { "cfg": { "type": "object", "properties": {} } },
            },
        });
        assert!(matches!(
            elicitation_form(&nested),
            ElicitationForm::Unsupported(reason)
                if reason.contains("cfg") && reason.contains("object")
        ));
        let loose_array = json!({
            "requestedSchema": {
                "type": "object",
                "properties": { "tags": { "type": "array", "items": { "type": "string" } } },
            },
        });
        assert!(matches!(
            elicitation_form(&loose_array),
            ElicitationForm::Unsupported(reason) if reason.contains("enum")
        ));
        let empty = json!({
            "requestedSchema": { "type": "object", "properties": {} },
        });
        assert!(matches!(
            elicitation_form(&empty),
            ElicitationForm::Unsupported(reason) if reason.contains("asks nothing")
        ));
    }

    #[test]
    fn elicitation_declines_missing_required_and_cancels_on_no_answer() {
        let params = json!({
            "message": "Name it",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "title": "Name" },
                    "note": { "type": "string", "title": "Note" },
                },
                "required": ["name"],
            },
        });
        let form = elicitation_form(&params);
        assert_eq!(
            elicitation_response(&form, &[]),
            json!({ "action": "cancel" }),
            "a dropped/unanswered form is never consent"
        );
        assert_eq!(
            elicitation_response(&form, &[answer("note", &["hi"])]),
            json!({ "action": "decline" }),
            "a missing required field declines rather than inventing a value"
        );
        let optional_only = elicitation_form(&json!({
            "requestedSchema": {
                "type": "object",
                "properties": { "note": { "type": "string", "title": "Note" } },
            },
        }));
        assert_eq!(
            elicitation_response(&optional_only, &[answer("note", &["hi"])]),
            json!({ "action": "accept", "content": { "note": "hi" } })
        );
    }

    #[test]
    fn elicitation_free_text_is_capped() {
        let params = json!({
            "requestedSchema": {
                "type": "object",
                "properties": { "story": { "type": "string", "title": "Story" } },
                "required": ["story"],
            },
        });
        let form = elicitation_form(&params);
        let long = "é".repeat(ELICITATION_TEXT_CAP);
        let response = elicitation_response(&form, &[typed("story", &long)]);
        let value = response["content"]["story"].as_str().unwrap();
        assert!(value.len() < long.len());
        assert!(value.ends_with("… [truncated]"));
    }
}
