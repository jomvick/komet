use komet_proto::{AgentEvent, ToolCall, UserInputQuestion};
use serde_json::Value;

pub fn normalize_line(line: &str) -> Option<AgentEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let t = v
        .get("text")
        .or_else(|| v.get("content"))
        .or_else(|| v.get("message"))?
        .as_str()?;
    Some(AgentEvent::TextDelta {
        text: t.to_string(),
    })
}

/// Normalize an Antigravity tool invocation to a typed `ToolCall`.
pub fn normalize_tool_call(name: &str, params: Option<&Value>) -> ToolCall {
    let p = params.unwrap_or(&Value::Null);

    match name {
        "run_command" | "bash" | "exec" => {
            let command = p
                .get("CommandLine")
                .or_else(|| p.get("command"))
                .or_else(|| p.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::Exec { command }
        }
        "view_file" | "read_file" => {
            let path = p
                .get("AbsolutePath")
                .or_else(|| p.get("path"))
                .or_else(|| p.get("file_path"))
                .or_else(|| p.get("TargetFile"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::ReadFile { path }
        }
        "write_to_file" | "create_file" => {
            let path = p
                .get("TargetFile")
                .or_else(|| p.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let content = p
                .get("CodeContent")
                .or_else(|| p.get("content"))
                .and_then(Value::as_str)
                .map(str::to_string);
            ToolCall::WriteFile { path, content }
        }
        "replace_file_content" | "edit_file" => {
            let path = p
                .get("TargetFile")
                .or_else(|| p.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let old_string = p
                .get("TargetContent")
                .or_else(|| p.get("old_string"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let new_string = p
                .get("ReplacementContent")
                .or_else(|| p.get("new_string"))
                .and_then(Value::as_str)
                .map(str::to_string);
            ToolCall::EditFile {
                path,
                old_string,
                new_string,
            }
        }
        "grep_search" | "search" => {
            let pattern = p
                .get("Query")
                .or_else(|| p.get("pattern"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let path = p
                .get("SearchPath")
                .or_else(|| p.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string);
            ToolCall::Search { pattern, path }
        }
        "find_by_name" | "glob" => {
            let pattern = p
                .get("Pattern")
                .or_else(|| p.get("pattern"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::Glob { pattern }
        }
        "search_web" => {
            let query = p
                .get("query")
                .or_else(|| p.get("Query"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::WebSearch { query }
        }
        "read_url_content" => {
            let url = p
                .get("Url")
                .or_else(|| p.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::WebFetch { url, prompt: None }
        }
        "call_mcp_tool" => {
            let server = p
                .get("ServerName")
                .or_else(|| p.get("server"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool = p
                .get("ToolName")
                .or_else(|| p.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = p
                .get("Arguments")
                .or_else(|| p.get("input"))
                .or_else(|| p.get("parameters"))
                .cloned();
            ToolCall::Mcp {
                server,
                tool,
                input,
            }
        }
        "invoke_subagent" => {
            // Check if subagents role is specified to follow "Agent: <Role>" convention
            let role = p
                .get("Subagents")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|first| first.get("Role"))
                .and_then(Value::as_str)
                .unwrap_or("Subagent");
            let name = format!("Agent: {role}");
            ToolCall::Unknown {
                name,
                input: params.cloned(),
            }
        }
        other => ToolCall::Unknown {
            name: other.to_string(),
            input: params.cloned(),
        },
    }
}

/// Extract questions from an `ask_question` tool call parameter object.
pub fn extract_questions(params: &Value) -> Option<Vec<UserInputQuestion>> {
    let questions_array = params.get("questions").and_then(Value::as_array)?;
    if questions_array.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for (i, item) in questions_array.iter().enumerate() {
        let question = item.get("question").and_then(Value::as_str)?.to_string();
        let options = item
            .get("options")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let multi_select = item
            .get("is_multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        out.push(UserInputQuestion {
            id: format!("agy-q-{i}"),
            header: "Antigravity".into(),
            question,
            options,
            multi_select,
        });
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_exec() {
        let json = serde_json::json!({ "CommandLine": "cargo test", "Cwd": "/app" });
        let call = normalize_tool_call("run_command", Some(&json));
        match call {
            ToolCall::Exec { command } => assert_eq!(command, "cargo test"),
            _ => panic!("expected Exec tool call"),
        }
    }

    #[test]
    fn test_normalize_view_file() {
        let json = serde_json::json!({ "AbsolutePath": "/path/to/file.rs" });
        let call = normalize_tool_call("view_file", Some(&json));
        match call {
            ToolCall::ReadFile { path } => assert_eq!(path, "/path/to/file.rs"),
            _ => panic!("expected ReadFile tool call"),
        }
    }

    #[test]
    fn test_normalize_subagent() {
        let json = serde_json::json!({
            "Subagents": [
                { "Role": "Codebase Researcher", "TypeName": "research", "Prompt": "Search files" }
            ]
        });
        let call = normalize_tool_call("invoke_subagent", Some(&json));
        assert!(call.is_subagent_spawn());
        match call {
            ToolCall::Unknown { name, .. } => assert_eq!(name, "Agent: Codebase Researcher"),
            _ => panic!("expected Unknown with subagent name"),
        }
    }

    #[test]
    fn test_extract_questions() {
        let json = serde_json::json!({
            "questions": [
                {
                    "question": "Which option?",
                    "options": ["First", "Second"],
                    "is_multi_select": false
                }
            ]
        });
        let questions = extract_questions(&json).expect("should extract questions");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Which option?");
        assert_eq!(questions[0].options, vec!["First", "Second"]);
        assert!(!questions[0].multi_select);
    }
}
