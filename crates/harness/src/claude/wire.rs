//! Claude CLI stream-json wire frames (stdout JSONL + stdin lines).
//!
//! Tolerant by construction: every field defaults, unknown frame types map to
//! [`Frame::Other`], so a newer CLI never breaks parsing — we only read the
//! fields the normalizer needs (spec: docs/research/harness.md).

use serde::Deserialize;
use serde_json::{Value, json};

/// One parsed stdout line.
#[derive(Debug)]
pub(crate) enum Frame {
    System(SystemFrame),
    StreamEvent(StreamEventFrame),
    Assistant(MessageFrame),
    User(MessageFrame),
    RateLimit(RateLimitFrame),
    Result(ResultFrame),
    ControlRequest(ControlRequestFrame),
    /// A `/clear` sent as a prompt line resets the session BEFORE the next
    /// `system:init` — live-verified 2026-08-31 (see
    /// docs/research/slash-commands-inventory.md): the wire is
    /// `conversation_reset` → a fresh `system:init` (new `session_id`) →
    /// the CLI's own confirmation turn. Carries no model/tools/cwd of its
    /// own; it exists only to tell the normalizer the NEXT init is a genuine
    /// new session, not the dedup-worthy kind a background-subagent wake
    /// turn re-sends with the SAME session id.
    ConversationReset(#[allow(dead_code)] ConversationResetFrame),
    /// control_response / control_cancel_request / anything unknown.
    Other,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SystemFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub session_id: String,
    /// `task_notification` (a background subagent settling): the spawning
    /// Agent tool's id — the only TAGGED terminal signal the 2.1.x wire has
    /// for a background subagent (its frames otherwise just stop).
    #[serde(default, alias = "toolUseId")]
    pub tool_use_id: Option<String>,
    /// `task_notification` terminal status (`completed`/`failed`/`killed`…).
    #[serde(default)]
    pub status: Option<String>,
    /// `task_started`: the agent/task id (`SendMessage`'s `to:` address) —
    /// with `tool_use_id`, the agentId→spawn mapping steers need.
    #[serde(default, alias = "taskId")]
    pub task_id: Option<String>,
    /// `task_started`: present only for AGENT tasks (a subagent spawning),
    /// absent on subagent-owned background shell tasks.
    #[serde(default)]
    pub subagent_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ConversationResetFrame {
    #[serde(default)]
    pub new_conversation_id: String,
    #[serde(default)]
    pub session_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamEventFrame {
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub event: StreamEventBody,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamEventBody {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub delta: Delta,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Delta {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub thinking: String,
}

/// An `assistant` or `user` frame (an Anthropic API message envelope).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageFrame {
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub message: MessageBody,
    /// Terse assistant-level error code (`rate_limit`, `billing_error`, …).
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageBody {
    /// Either a plain string or an array of content blocks.
    #[serde(default)]
    pub content: Value,
}

impl MessageBody {
    pub fn blocks(&self) -> impl Iterator<Item = ContentBlock> + '_ {
        self.content
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|b| serde_json::from_value(b.clone()).ok())
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ContentBlock {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub tool_use_id: String,
    #[serde(default)]
    pub is_error: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitFrame {
    #[serde(default)]
    pub rate_limit_info: RateLimitInfo,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitInfo {
    #[serde(default)]
    pub status: String,
    #[serde(rename = "rateLimitType", default)]
    pub rate_limit_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResultFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub errors: Vec<Value>,
    #[serde(default)]
    pub usage: UsageBody,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct UsageBody {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// A CLI→client control request (`can_use_tool` is the one we act on).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlRequestFrame {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub request: ControlRequestBody,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlRequestBody {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub input: Value,
}

/// Parse one stdout JSONL line. `Err` = not JSON; unknown types = `Other`.
pub(crate) fn parse_frame(line: &str) -> Result<Frame, serde_json::Error> {
    let value: Value = serde_json::from_str(line)?;
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let frame = match kind {
        "system" => Frame::System(serde_json::from_value(value)?),
        "stream_event" => Frame::StreamEvent(serde_json::from_value(value)?),
        "assistant" => Frame::Assistant(serde_json::from_value(value)?),
        "user" => Frame::User(serde_json::from_value(value)?),
        "rate_limit_event" => Frame::RateLimit(serde_json::from_value(value)?),
        "result" => Frame::Result(serde_json::from_value(value)?),
        "control_request" => Frame::ControlRequest(serde_json::from_value(value)?),
        "conversation_reset" => Frame::ConversationReset(serde_json::from_value(value)?),
        _ => Frame::Other,
    };
    Ok(frame)
}

/// A stdin user turn: `{"type":"user","message":{...},"parent_tool_use_id":null}`.
/// Steering = another such line mid-run (consumed at a step boundary).
pub(crate) fn user_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// One inline image for a stdin user turn (Anthropic base64 image source).
pub(crate) struct ImageBlock {
    /// One of the API-supported media types (png/jpeg/gif/webp).
    pub media_type: String,
    /// Raw base64 (no data-URL prefix).
    pub data: String,
}

/// A stdin user turn whose content is an array of blocks: the attached images
/// first, then the text — the standard Anthropic image+text message shape
/// (verified against the real CLI: `--input-format stream-json` accepts image
/// content blocks in user frames). Empty `images` degrades to the plain line.
pub(crate) fn user_message_line_with_images(text: &str, images: &[ImageBlock]) -> String {
    if images.is_empty() {
        return user_message_line(text);
    }
    let mut blocks: Vec<Value> = images
        .iter()
        .map(|img| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.media_type,
                    "data": img.data,
                },
            })
        })
        .collect();
    blocks.push(json!({ "type": "text", "text": text }));
    json!({
        "type": "user",
        "message": { "role": "user", "content": blocks },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// Success reply to a CLI control request (`can_use_tool` allow/deny payloads).
pub(crate) fn control_response_line(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
    .to_string()
}

/// `can_use_tool` allow payload with the (possibly updated) tool input.
pub(crate) fn allow_response(updated_input: Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": updated_input })
}

/// `can_use_tool` deny payload.
pub(crate) fn deny_response(message: Option<&str>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("behavior".into(), "deny".into());
    if let Some(msg) = message {
        obj.insert("message".into(), msg.into());
    }
    Value::Object(obj)
}

/// Client→CLI interrupt control request.
pub(crate) fn interrupt_request_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    })
    .to_string()
}

/// Client→CLI get_context_usage control request (Bucket C).
pub(crate) fn context_usage_request_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "get_context_usage" },
    })
    .to_string()
}

/// Client→CLI set_permission_mode control request (Bucket C).
#[allow(dead_code)]
pub(crate) fn set_permission_mode_request_line(request_id: &str, mode: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "set_permission_mode", "mode": mode },
    })
    .to_string()
}

/// Client→CLI set_model control request (Bucket C).
#[allow(dead_code)]
pub(crate) fn set_model_request_line(request_id: &str, model: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "set_model", "model": model },
    })
    .to_string()
}

/// Client→CLI stop_task control request (Bucket C).
#[allow(dead_code)]
pub(crate) fn stop_task_request_line(request_id: &str, task_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "stop_task", "task_id": task_id },
    })
    .to_string()
}

/// Client→CLI rewind_files control request (Bucket C).
#[allow(dead_code)]
pub(crate) fn rewind_files_request_line(request_id: &str, checkpoint: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "rewind_files", "checkpoint": checkpoint },
    })
    .to_string()
}

fn unwrap_response_payload(value: &Value) -> &Value {
    let mut payload = value;
    while let Some(inner) = payload.get("response") {
        payload = inner;
    }
    payload
}

/// Extract structured context usage from a get_context_usage response.
pub(crate) fn parse_context_usage_response(response: &Value) -> komet_proto::ContextUsage {
    let payload = unwrap_response_payload(response);
    let mut usage = komet_proto::ContextUsage {
        total_tokens: payload.get("totalTokens").and_then(Value::as_u64).unwrap_or(0),
        max_tokens: payload.get("maxTokens").and_then(Value::as_u64).unwrap_or(0),
        raw_max_tokens: payload.get("rawMaxTokens").and_then(Value::as_u64),
        percentage: payload.get("percentage").and_then(Value::as_f64),
        model: payload.get("model").and_then(Value::as_str).map(str::to_owned),
        ..Default::default()
    };

    if let Some(cats) = payload.get("categories").and_then(Value::as_array) {
        for c in cats {
            if let Some(name) = c.get("name").and_then(Value::as_str) {
                usage.categories.push(komet_proto::ContextTokenCategory {
                    name: name.to_owned(),
                    tokens: c.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                    color: c.get("color").and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
    }

    if let Some(files) = payload.get("memoryFiles").and_then(Value::as_array) {
        for f in files {
            if let Some(path) = f.get("path").and_then(Value::as_str) {
                usage.memory_files.push(komet_proto::ContextMemoryFile {
                    path: path.to_owned(),
                    kind: f.get("type").and_then(Value::as_str).map(str::to_owned),
                    tokens: f.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                });
            }
        }
    }

    if let Some(tools) = payload.get("mcpTools").and_then(Value::as_array) {
        for t in tools {
            if let Some(name) = t.get("name").and_then(Value::as_str) {
                usage.mcp_tools.push(komet_proto::ContextMcpTool {
                    name: name.to_owned(),
                    server_name: t.get("serverName").and_then(Value::as_str).map(str::to_owned),
                    tokens: t.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                    is_loaded: t.get("isLoaded").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
    }

    if let Some(frontmatter) = payload
        .get("skills")
        .and_then(|s| s.get("skillFrontmatter"))
        .and_then(Value::as_array)
    {
        for s in frontmatter {
            if let Some(name) = s.get("name").and_then(Value::as_str) {
                usage.skills.push(komet_proto::SkillFrontmatterItem {
                    name: name.to_owned(),
                    source: s.get("source").and_then(Value::as_str).unwrap_or("userSettings").to_owned(),
                    tokens: s.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                    description: s.get("description").and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
    }

    usage
}

/// Parse skills from get_context_usage response into SlashCommands.
pub(crate) fn parse_skills_frontmatter(response: &Value) -> Vec<komet_proto::SlashCommand> {
    let payload = unwrap_response_payload(response);
    let Some(frontmatter) = payload
        .get("skills")
        .and_then(|s| s.get("skillFrontmatter"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    frontmatter
        .iter()
        .filter_map(|s| {
            let name = s.get("name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            let source = s.get("source").and_then(Value::as_str).unwrap_or("");
            let desc = s.get("description").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty());
            let description = match (desc, source) {
                (Some(d), _) => d.to_owned(),
                (None, "built-in") => format!("Bundled skill ({name})"),
                (None, "userSettings") => format!("Custom skill ({name})"),
                (None, other) if !other.is_empty() => format!("Skill ({other})"),
                (None, _) => format!("Skill ({name})"),
            };
            Some(komet_proto::SlashCommand {
                name: name.to_owned(),
                description,
                input_hint: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_and_unknown_frames() {
        let init = r#"{"type":"system","subtype":"init","model":"m","tools":["Bash"],"cwd":"/x","session_id":"s1"}"#;
        match parse_frame(init).expect("parses") {
            Frame::System(f) => {
                assert_eq!(f.subtype, "init");
                assert_eq!(f.session_id, "s1");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(matches!(
            parse_frame(r#"{"type":"mystery_frame"}"#).expect("parses"),
            Frame::Other
        ));
        assert!(parse_frame("not json").is_err());
    }

    /// Live-verified 2026-08-31: `/clear` sent as a prompt line produces this
    /// frame BEFORE the fresh `system:init` — see
    /// docs/research/slash-commands-inventory.md.
    #[test]
    fn parses_conversation_reset() {
        let raw = r#"{"type":"conversation_reset","new_conversation_id":"c2","session_id":"s2"}"#;
        match parse_frame(raw).expect("parses") {
            Frame::ConversationReset(f) => {
                assert_eq!(f.new_conversation_id, "c2");
                assert_eq!(f.session_id, "s2");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn user_line_shape_matches_protocol() {
        let line = user_message_line("hi");
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"], "hi");
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn user_line_with_images_is_blocks_then_text() {
        let line = user_message_line_with_images(
            "what is this?",
            &[ImageBlock {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["type"], "user");
        let content = v["message"]["content"].as_array().expect("array content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this?");
        // No images ⇒ identical to the plain string line.
        assert_eq!(
            user_message_line_with_images("hi", &[]),
            user_message_line("hi")
        );
    }

    #[test]
    fn control_request_lines_match_protocol() {
        let perm = set_permission_mode_request_line("r1", "default");
        let v: Value = serde_json::from_str(&perm).expect("json");
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request_id"], "r1");
        assert_eq!(v["request"]["subtype"], "set_permission_mode");
        assert_eq!(v["request"]["mode"], "default");

        let model = set_model_request_line("r2", "sonnet");
        let v: Value = serde_json::from_str(&model).expect("json");
        assert_eq!(v["request"]["subtype"], "set_model");
        assert_eq!(v["request"]["model"], "sonnet");

        let stop = stop_task_request_line("r3", "task-99");
        let v: Value = serde_json::from_str(&stop).expect("json");
        assert_eq!(v["request"]["subtype"], "stop_task");
        assert_eq!(v["request"]["task_id"], "task-99");

        let rewind = rewind_files_request_line("r4", "latest");
        let v: Value = serde_json::from_str(&rewind).expect("json");
        assert_eq!(v["request"]["subtype"], "rewind_files");
        assert_eq!(v["request"]["checkpoint"], "latest");

        let ctx = context_usage_request_line("r5");
        let v: Value = serde_json::from_str(&ctx).expect("json");
        assert_eq!(v["request"]["subtype"], "get_context_usage");
    }

    #[test]
    fn parses_context_usage_and_skills() {
        let raw = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "r1",
                "response": {
                    "totalTokens": 5000,
                    "maxTokens": 200000,
                    "percentage": 2.5,
                    "model": "claude-sonnet-4-6",
                    "categories": [{"name": "Skills", "tokens": 100, "color": "warning"}],
                    "memoryFiles": [{"path": "/tmp/CLAUDE.md", "type": "User", "tokens": 50}],
                    "mcpTools": [{"name": "grep", "serverName": "code", "tokens": 20, "isLoaded": true}],
                    "skills": {
                        "skillFrontmatter": [
                            {"name": "test-skill", "source": "userSettings", "tokens": 10},
                            {"name": "dataviz", "source": "built-in", "tokens": 300}
                        ]
                    }
                }
            }
        });
        let usage = parse_context_usage_response(&raw);
        assert_eq!(usage.total_tokens, 5000);
        assert_eq!(usage.max_tokens, 200000);
        assert_eq!(usage.categories.len(), 1);
        assert_eq!(usage.memory_files.len(), 1);
        assert_eq!(usage.mcp_tools.len(), 1);
        assert_eq!(usage.skills.len(), 2);

        let skills = parse_skills_frontmatter(&raw);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].description, "Custom skill (test-skill)");
        assert_eq!(skills[1].name, "dataviz");
        assert_eq!(skills[1].description, "Bundled skill (dataviz)");
    }
}
