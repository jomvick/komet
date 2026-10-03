use komet_proto::{AgentEvent, ToolCall, TodoItem, UserInputQuestion};
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

/// Extract `TodoItem`s from a params value carrying `todos: [{content, status}]`.
///
/// Returns `Some` (possibly empty) when the `todos` key exists with an array
/// where every element has a `content` string, `None` otherwise (so the
/// shape-detection fallback only fires when the params actually look like a
/// todo call, not on arbitrary objects that happen to have unrelated fields).
fn todo_items(params: Option<&Value>) -> Option<Vec<TodoItem>> {
    let items = params?.get("todos")?.as_array()?;
    // Require every element to have a `content` string — this is the
    // discriminant that distinguishes a real todo list from other array params.
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
                done: t.get("status").and_then(Value::as_str) == Some("completed")
                    || t.get("completed").and_then(Value::as_bool) == Some(true),
            })
            .collect(),
    )
}

/// Normalize an Antigravity tool invocation to a typed `ToolCall`.
pub fn normalize_tool_call(name: &str, params: Option<&Value>) -> ToolCall {
    let p = params.unwrap_or(&Value::Null);

    match name {
        "run_command" | "bash" | "exec" | "command_status" | "send_command_input"
        | "notebook_execution" => {
            let command = p
                .get("CommandLine")
                .or_else(|| p.get("command"))
                .or_else(|| p.get("cmd"))
                .or_else(|| p.get("Input"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::Exec { command }
        }
        "view_file" | "read_file" | "read_resource" => {
            let path = p
                .get("AbsolutePath")
                .or_else(|| p.get("path"))
                .or_else(|| p.get("file_path"))
                .or_else(|| p.get("TargetFile"))
                .or_else(|| p.get("Uri"))
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
        "replace_file_content"
        | "edit_file"
        | "multi_replace_file_content"
        | "sed_file"
        | "notebook_edit" => {
            let path = p
                .get("TargetFile")
                .or_else(|| p.get("path"))
                .or_else(|| p.get("file_path"))
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
        "list_dir" => {
            let path = p
                .get("DirectoryPath")
                .or_else(|| p.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ToolCall::Glob { pattern: path }
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
        "read_url_content" | "open_browser_url" | "read_browser_page" => {
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
        // Explicit todo-write tools: Antigravity / AGY emits these when the
        // agent writes its task plan. Kept after invoke_subagent so spawns win.
        "TodoWrite" | "todo_write" | "write_todo" | "update_todos" => ToolCall::Todo {
            items: todo_items(params).unwrap_or_default(),
        },
        // Shape-detection fallback: any tool not matched above whose params
        // carry a well-formed `todos` array is normalized to a Todo chip
        // rather than falling through to an Unknown JSON dump. This makes the
        // feature resilient to future tool-name changes in AGY without
        // requiring a code change.
        other => {
            if let Some(items) = todo_items(params) {
                ToolCall::Todo { items }
            } else {
                ToolCall::Unknown {
                    name: other.to_string(),
                    input: params.cloned(),
                }
            }
        }
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
    fn test_normalize_list_dir() {
        let json = serde_json::json!({ "DirectoryPath": "/home/user/project" });
        let call = normalize_tool_call("list_dir", Some(&json));
        match call {
            ToolCall::Glob { pattern } => assert_eq!(pattern, "/home/user/project"),
            _ => panic!("expected Glob tool call for list_dir"),
        }
    }

    #[test]
    fn test_normalize_edit_file() {
        let json = serde_json::json!({
            "TargetFile": "/app/main.rs",
            "TargetContent": "old",
            "ReplacementContent": "new"
        });
        let call = normalize_tool_call("multi_replace_file_content", Some(&json));
        match call {
            ToolCall::EditFile {
                path,
                old_string,
                new_string,
            } => {
                assert_eq!(path, "/app/main.rs");
                assert_eq!(old_string.as_deref(), Some("old"));
                assert_eq!(new_string.as_deref(), Some("new"));
            }
            _ => panic!("expected EditFile tool call"),
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

    // ── Todo normalization ────────────────────────────────────────────────────

    #[test]
    fn agy_todo_write_explicit_name_maps_to_todo_chip() {
        // AGY native name with content/status shape.
        let json = serde_json::json!({
            "todos": [
                { "content": "Set up repo", "status": "completed", "priority": "high" },
                { "content": "Write tests",  "status": "in_progress", "priority": "medium" },
            ]
        });
        let call = normalize_tool_call("TodoWrite", Some(&json));
        match call {
            ToolCall::Todo { items } => {
                assert_eq!(items.len(), 2);
                assert!(items[0].done);
                assert!(!items[1].done);
                assert_eq!(items[0].text, "Set up repo");
            }
            other => panic!("expected Todo, got {other:?}"),
        }
    }

    #[test]
    fn agy_todo_write_alias_todo_write_maps_to_todo_chip() {
        // snake_case alias.
        let json = serde_json::json!({
            "todos": [{ "content": "Deploy", "status": "pending" }]
        });
        let call = normalize_tool_call("todo_write", Some(&json));
        assert!(matches!(call, ToolCall::Todo { .. }));
    }

    #[test]
    fn agy_unknown_tool_with_todo_shape_maps_to_todo_chip() {
        // Future / renamed tool: unknown name but well-formed todos array.
        let json = serde_json::json!({
            "todos": [
                { "content": "a", "status": "completed" },
                { "content": "b", "status": "open" },
            ]
        });
        let call = normalize_tool_call("some_future_plan_tool", Some(&json));
        match call {
            ToolCall::Todo { items } => {
                assert_eq!(items.len(), 2);
                assert!(items[0].done);
                assert!(!items[1].done);
            }
            other => panic!("expected Todo via shape-detection, got {other:?}"),
        }
    }

    #[test]
    fn agy_unknown_tool_without_todo_shape_stays_unknown() {
        // A tool that has an array but NOT a `todos` key — must not become Todo.
        let json = serde_json::json!({ "items": [{ "id": 1 }] });
        let call = normalize_tool_call("some_other_tool", Some(&json));
        assert!(matches!(call, ToolCall::Unknown { .. }));
    }

    #[test]
    fn agy_todo_write_explicit_name_with_malformed_items_yields_empty_todo() {
        // Malformed todos (no `content`) must not produce a Todo chip.
        let json = serde_json::json!({
            "todos": [{ "text": "oops", "status": "pending" }]
        });
        let call = normalize_tool_call("TodoWrite", Some(&json));
        // `todo_items` returns None → falls back to empty vec via unwrap_or_default
        // for the named arm, so still Todo but empty items (acceptable — rather
        // than Unknown JSON dump).
        assert!(matches!(call, ToolCall::Todo { .. }));
    }
}
