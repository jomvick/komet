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
        MessageRole::Assistant | MessageRole::System => {
            out.push_str(&rendered_blocks(&entry.parts).join("\n\n"));
        }
    }
    out
}

/// Every export block a list of parts carries. Shared by the assistant role and
/// the SYSTEM role on purpose: a compaction that happened between two turns
/// lives in a system entry, and an entry that exports to nothing is exactly the
/// invisible transcript this exists to prevent. Parts with nothing to say
/// (a switch marker, a permission row) contribute no block.
fn rendered_blocks(parts: &[MessagePart]) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for p in parts {
        match p {
            MessagePart::Text { text, .. } => blocks.push(text.clone()),
            MessagePart::Tool {
                call,
                output,
                is_error,
                ..
            } => blocks.push(tool_call_line(call, output.as_deref(), *is_error)),
            MessagePart::Error { message, .. } => blocks.push(format!("⚠ {message}")),
            // A compaction boundary: facts about the squeeze, and its
            // summary text ONLY when the doc itself carries it (the
            // opt-in policy). A journal-held summary is announced, never
            // quoted — an export leaves the machine, and a summary is
            // model prose about the whole conversation.
            MessagePart::Compaction {
                status,
                trigger,
                tokens_before,
                tokens_after,
                error,
                summary,
                summary_bytes,
                summary_in_journal,
                ..
            } => {
                let mut line = String::from("- [context ");
                line.push_str(match status {
                    komet_proto::CompactionStatus::InProgress => "compacting",
                    komet_proto::CompactionStatus::Completed => "compacted",
                    komet_proto::CompactionStatus::Failed => "compaction failed",
                    komet_proto::CompactionStatus::Cancelled => "compaction cancelled",
                });
                match (tokens_before, tokens_after) {
                    (Some(before), Some(after)) => {
                        line.push_str(&format!(": {before} → {after} tokens"))
                    }
                    (Some(before), None) => line.push_str(&format!(": {before} tokens in")),
                    _ => {}
                }
                if let Some(trigger) = trigger {
                    line.push_str(&format!(" ({trigger})"));
                }
                if let Some(error) = error {
                    line.push_str(&format!(" — {error}"));
                }
                line.push(']');
                match summary {
                    Some(text) => line.push_str(&format!("\n\n> {}", text.replace('\n', "\n> "))),
                    None if *summary_in_journal => {
                        let size = summary_bytes
                            .map(|bytes| format!("{bytes} bytes"))
                            .unwrap_or_else(|| "of unknown size".into());
                        line.push_str(&format!(
                            " — summary {size} in the host's run journal, not exported"
                        ));
                    }
                    None => {}
                }
                blocks.push(line);
            }
            // Questions and the answers users gave them. A secret
            // question's answer never travels: the field exists to hold
            // a token or other credential, and an export is a file
            // someone may share.
            MessagePart::Input {
                questions,
                responses,
                ..
            } => {
                let mut block = String::new();
                for question in questions {
                    block.push_str(&format!("Q [{}]: {}\n", question.header, question.question));
                }
                for response in responses {
                    let answers = response
                        .answers
                        .iter()
                        .map(|answer| {
                            let secret = questions
                                .iter()
                                .any(|q| q.id == answer.question_id && q.secret);
                            if secret {
                                return komet_proto::WITHHELD_SECRET_ANSWER.to_owned();
                            }
                            [
                                answer.free_text.clone().unwrap_or_default(),
                                answer.labels.join(", "),
                            ]
                            .into_iter()
                            .filter(|part| !part.trim().is_empty())
                            .collect::<Vec<_>>()
                            .join(", ")
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    block.push_str(&format!(
                        "A ({}, {}): {}\n",
                        response.device,
                        if response.applied {
                            "delivered"
                        } else {
                            "not delivered"
                        },
                        if answers.is_empty() {
                            "(no answer)"
                        } else {
                            &answers
                        }
                    ));
                }
                blocks.push(block.trim_end().to_owned());
            }
            _ => {}
        }
    }
    blocks
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
        let e = entry(MessageRole::User, vec![text("p1", "do # stuff *now*")]);
        assert_eq!(
            entry_to_markdown(&e, false),
            "**User:**\n\n> do # stuff *now*"
        );
    }

    #[test]
    fn assistant_text_passes_through_raw() {
        let e = entry(MessageRole::Assistant, vec![text("p1", "# Done\n\n* hi")]);
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

    fn compaction(status: komet_proto::CompactionStatus, summary: Option<&str>) -> MessagePart {
        MessagePart::Compaction {
            id: "cmp-c1".into(),
            compaction_id: "c1".into(),
            status,
            trigger: Some("auto".into()),
            tokens_before: Some(165_000),
            tokens_after: Some(12_000),
            error: None,
            summary: summary.map(str::to_owned),
            summary_bytes: Some(2_048),
            summary_in_journal: true,
        }
    }

    #[test]
    fn export_honours_summary_and_secret_policies() {
        use komet_proto::{CompactionStatus, UserInputAnswer, UserInputQuestion};

        // Journal-only (the default): the boundary is in the export, its text
        // is not, and the reader is told a fetchable summary exists.
        let md = entry_to_markdown(
            &entry(
                MessageRole::System,
                vec![compaction(CompactionStatus::Completed, None)],
            ),
            false,
        );
        assert!(
            md.contains("- [context compacted: 165000 → 12000 tokens (auto)]"),
            "{md}"
        );
        assert!(
            md.contains("2048 bytes in the host's run journal, not exported"),
            "the export must not silently drop a summary the reader could fetch: {md}"
        );

        // Opt-in SummaryInDoc: the text the doc actually carries does export.
        let md = entry_to_markdown(
            &entry(
                MessageRole::Assistant,
                vec![compaction(
                    CompactionStatus::Completed,
                    Some("recapped: they met at noon"),
                )],
            ),
            false,
        );
        assert!(md.contains("> recapped: they met at noon"), "{md}");

        // A failed boundary exports its reason, not a token arrow.
        let mut failed = compaction(CompactionStatus::Failed, None);
        if let MessagePart::Compaction {
            error,
            tokens_after,
            summary,
            summary_in_journal,
            ..
        } = &mut failed
        {
            *error = Some("the run died".into());
            *tokens_after = None;
            *summary = None;
            *summary_in_journal = false;
        }
        let md = entry_to_markdown(&entry(MessageRole::System, vec![failed]), false);
        assert!(
            md.contains("compaction failed: 165000 tokens in (auto) — the run died"),
            "{md}"
        );
        assert!(
            !md.contains("not exported"),
            "there is nothing to fetch: {md}"
        );

        // Answers: the visible one travels, the secret one never does — not
        // even as free text, because that field is where a token goes.
        let question = |id: &str, secret: bool| UserInputQuestion {
            id: id.into(),
            header: "Auth".into(),
            question: format!("Q{id}"),
            options: vec!["b".into()],
            multi_select: false,
            option_descriptions: vec![],
            allow_free_text: true,
            secret,
            default: None,
        };
        let answered = MessagePart::Input {
            id: "r1".into(),
            request_id: "r1".into(),
            questions: vec![question("q1", false), question("q2", true)],
            resolved: true,
            responses: vec![crate::parts::InputResponse {
                answers: vec![
                    UserInputAnswer {
                        question_id: "q1".into(),
                        labels: vec!["b".into()],
                        free_text: None,
                    },
                    UserInputAnswer {
                        question_id: "q2".into(),
                        labels: vec![],
                        free_text: Some("hunter2".into()),
                    },
                ],
                decided_at: 1_700_000_000_000,
                device: "dev-a".into(),
                applied: true,
            }],
            source: komet_proto::QuestionSource::Native,
        };
        let md = entry_to_markdown(&entry(MessageRole::Assistant, vec![answered]), false);
        assert!(md.contains("Q [Auth]: Qq1") && md.contains("Qq2"), "{md}");
        assert!(md.contains("(dev-a, delivered): b"), "{md}");
        assert!(
            !md.contains("hunter2"),
            "a secret answer reached the export: {md}"
        );
        assert!(md.contains("[withheld: secret answer]"), "{md}");
    }
}
