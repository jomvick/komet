//! Markdown export — pure derivation over [`crate::SessionMessageEntry`].

use komet_proto::{ToolCall, view::tool_chip_content};

use crate::parts::MessagePart;
use crate::schema::{MessageRole, SessionMessageEntry};

/// One-line export for a tool part: chip label + stored summary.
///
/// Reuses [`tool_chip_content`] (the same label/detail the renderer shows)
/// and the `output` summary already persisted by `summarize_tool_output` —
/// nothing is recomputed from raw JSON.
pub fn tool_call_line(call: &ToolCall, output: Option<&str>, is_error: bool) -> String {
    let (label, detail) = tool_chip_content(call);
    let result = match output {
        None => "(pending)".to_string(),
        Some(out) if is_error => format!("⚠ {out}"),
        Some(out) => out.to_string(),
    };
    if detail.trim().is_empty() {
        format!("- `{label}` → {result}")
    } else {
        format!("- `{label} {detail}` → {result}")
    }
}

fn timestamp_prefix(entry: &SessionMessageEntry) -> String {
    format!("<!-- {} -->\n", entry.created_at)
}

/// Export a single entry. User text is verbatim inside a blockquote so
/// `#`/`*` typed by the user never becomes formatting on render;
/// assistant `Text` passes through raw (already markdown).
pub fn entry_to_markdown(entry: &SessionMessageEntry, with_timestamps: bool) -> String {
    let mut out = String::new();
    if with_timestamps {
        out.push_str(&timestamp_prefix(entry));
    }
    match entry.role {
        MessageRole::User => {
            out.push_str("**User:**\n\n");
            let body: Vec<String> = entry
                .parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.clone()),
                    MessagePart::Error { message, .. } => Some(message.clone()),
                    _ => None,
                })
                .collect();
            let quoted = body
                .join("\n\n")
                .lines()
                .map(|l| {
                    if l.trim().is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {l}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&quoted);
        }
        MessageRole::Assistant => {
            let mut blocks: Vec<String> = Vec::new();
            for p in &entry.parts {
                match p {
                    MessagePart::Text { text, .. } => blocks.push(text.clone()),
                    MessagePart::Tool {
                        call,
                        output,
                        is_error,
                        ..
                    } => blocks.push(tool_call_line(call, output.as_deref(), *is_error)),
                    MessagePart::Error { message, .. } => {
                        blocks.push(format!("⚠ {message}"));
                    }
                    _ => {}
                }
            }
            out.push_str(&blocks.join("\n\n"));
        }
        MessageRole::System => {
            for p in &entry.parts {
                if let MessagePart::Text { text, .. } = p {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Export a whole transcript, entries joined with `---` separators.
pub fn transcript_to_markdown(entries: &[SessionMessageEntry], with_timestamps: bool) -> String {
    entries
        .iter()
        .map(|e| entry_to_markdown(e, with_timestamps))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[cfg(test)]
mod export_tests {
    use crate::parts::MessagePart;
    use crate::schema::{MessageRole, SessionMessageEntry};
    use komet_proto::ToolCall;

    use super::{entry_to_markdown, tool_call_line, transcript_to_markdown};

    fn entry(role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "m1".into(),
            role,
            parts,
            created_at: 1_700_000_000_000,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn text(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn tool_line_uses_chip_label_and_stored_summary() {
        let call = ToolCall::ReadFile {
            path: "src/main.rs".into(),
        };
        assert_eq!(
            tool_call_line(&call, Some("hello"), false),
            "- `Read src/main.rs` → hello"
        );
    }

    #[test]
    fn tool_line_marks_unresolved_and_errors() {
        let call = ToolCall::Exec {
            command: "cargo test".into(),
        };
        assert_eq!(
            tool_call_line(&call, None, false),
            "- `Run cargo test` → (pending)"
        );
        assert_eq!(
            tool_call_line(&call, Some("boom"), true),
            "- `Run cargo test` → ⚠ boom"
        );
    }

    #[test]
    fn user_text_exports_verbatim_as_blockquote() {
        let e = entry(
            MessageRole::User,
            vec![text("p1", "do # stuff *now*")],
        );
        assert_eq!(
            entry_to_markdown(&e, false),
            "**User:**\n\n> do # stuff *now*"
        );
    }

    #[test]
    fn assistant_text_passes_through_raw() {
        let e = entry(
            MessageRole::Assistant,
            vec![text("p1", "# Done\n\n* hi")],
        );
        assert_eq!(entry_to_markdown(&e, false), "# Done\n\n* hi");
    }

    #[test]
    fn transcript_joins_entries_with_separator() {
        let u = entry(MessageRole::User, vec![text("p1", "hi")]);
        let a = entry(MessageRole::Assistant, vec![text("p2", "hello")]);
        let md = transcript_to_markdown(&[u, a], false);
        assert!(md.contains("**User:**"));
        assert!(md.contains("hello"));
        assert!(md.contains("---"));
    }

    #[test]
    fn timestamps_toggle_adds_header_line() {
        let e = entry(MessageRole::User, vec![text("p1", "hi")]);
        let md = entry_to_markdown(&e, true);
        assert!(md.contains("<!--"));
    }
}
