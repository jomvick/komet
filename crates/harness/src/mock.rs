//! Mock harness for engine/UI tests: replays a scripted event sequence.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use komet_proto::{
    AgentEvent, DoneStatus, ErrorKind, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls};

pub struct MockHarness {
    pub script: Vec<AgentEvent>,
}

/// The scripted `question_multi` scenario (`KOMET_MOCK_QUESTION=multi`, also
/// accepted as `1`): two paged questions — a single-select and a multi-select —
/// each with a free-text ("Autre") field, so the wizard's pagination, checks
/// and free-text path all appear in one demo run.
fn question_script() -> Vec<UserInputQuestion> {
    vec![
        UserInputQuestion {
            id: "q-sync".into(),
            header: "Question".into(),
            question: "Which sync strategy should the rewrite use?".into(),
            options: vec![
                "Poll the doc host every 120ms".into(),
                "Event-driven fold with coalesced commits".into(),
                "Hybrid: event-driven with a polling fallback".into(),
            ],
            multi_select: false,
            option_descriptions: Vec::new(),
            allow_free_text: true,
            secret: false,
            default: None,
        },
        UserInputQuestion {
            id: "q-gates".into(),
            header: "Question".into(),
            question: "Which suites should gate the merge?".into(),
            options: vec![
                "Unit tests".into(),
                "End-to-end (two-device)".into(),
                "Golden screenshots".into(),
            ],
            multi_select: true,
            option_descriptions: Vec::new(),
            allow_free_text: true,
            secret: false,
            default: None,
        },
    ]
}

#[async_trait]
impl Harness for MockHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Mock"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![
            Model {
                id: "mock-1".into(),
                label: "Mock 1".into(),
                description: None,
                reasoning_levels: vec![ReasoningLevel::Medium],
                options: vec![],
            },
            // Claude-mirroring demo model: lets scripted runs carry the same
            // chip labels ("Fable 5 · High") as a real Claude session.
            Model {
                id: "mock-fable-5".into(),
                label: "Fable 5".into(),
                description: None,
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ],
                options: vec![],
            },
        ])
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        // Optional pacing knob for demos/manual testing: `KOMET_MOCK_DELAY_MS`
        // spaces the scripted events out so
        // live-run UI states (working indicator, streaming fade, trailing
        // tool-group auto-open) are observable. Unset (the default, and in
        // tests) streams instantly.
        let delay_ms = std::env::var("KOMET_MOCK_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let delay = std::time::Duration::from_millis(delay_ms);

        // Dev/testing knob: `KOMET_MOCK_QUESTION=multi` (or any non-empty,
        // non-`0` value) swaps in a run that asks the `question_multi`
        // scenario mid-stream via `controls.request_input` (the engine mints
        // the request id, emits `InputRequested`, and resolves it from the
        // `RespondInput` doc command) — the only data-side way to put the
        // question dock on screen. `scripts/dev-demo.sh --questions` sets it.
        let question_mode = std::env::var("KOMET_MOCK_QUESTION")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        if question_mode {
            let request_input = controls.request_input;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            tokio::spawn(async move {
                let pause = if delay_ms == 0 {
                    std::time::Duration::from_millis(50)
                } else {
                    delay
                };
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text:
                        "Before I wire the reconciliation path I need two decisions from you.\n\n"
                            .into(),
                });
                tokio::time::sleep(pause).await;
                let answers = request_input(question_script(), komet_proto::QuestionSource::Native)
                    .await
                    .unwrap_or_default();
                let picked: Vec<String> = answers
                    .iter()
                    .flat_map(|a| a.labels.iter().cloned())
                    .collect();
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text: format!(
                        "Locked in: **{}**. Proceeding with the plan.",
                        if picked.is_empty() {
                            "your defaults".to_string()
                        } else {
                            picked.join("**, **")
                        }
                    ),
                });
                let _ = tx.send(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                    reason: None,
                });
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            });
            return Ok(stream.boxed());
        }

        // Dev/testing knob: `KOMET_MOCK_REPEAT=N` loops the script body N times
        // before the final Done — long single-reply streams for frame-cost /
        // smoothness measurement (the terminal `Done` is emitted exactly once,
        // at the very end).
        let repeat = std::env::var("KOMET_MOCK_REPEAT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        // Dev/testing knob: `KOMET_MOCK_ERROR=1` appends a scripted error
        // before the terminal Done — the only data-side way to put the
        // transcript ErrorChip on screen with the mock harness.
        let mock_error = std::env::var("KOMET_MOCK_ERROR")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        // Dev/testing knob: `KOMET_MOCK_TABLE=1` appends scripted GFM tables
        // before the terminal Done — a plain 3-column grid plus a wide/uneven
        // one (long prose cell beside short cells, mixed alignment) for
        // table-styling checks against the reference app.
        let mock_table = std::env::var("KOMET_MOCK_TABLE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let done_ix = self
            .script
            .iter()
            .position(|e| matches!(e, AgentEvent::Done { .. }))
            .unwrap_or(self.script.len());
        let (body, tail) = self.script.split_at(done_ix);
        let error_event = mock_error.then(|| AgentEvent::Error {
            message: "Claude usage limit reached — try again after the limit resets.".into(),
            kind: ErrorKind::Unknown,
            detail: None,
            retryable: false,
        });
        // Dev/testing knob: `KOMET_MOCK_CODE=1` appends rust + ts code blocks
        // (keywords, strings, numbers, comments) plus inline code — for
        // syntax-palette and inline-code styling checks against the reference.
        let mock_code = std::env::var("KOMET_MOCK_CODE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let code_event = mock_code.then(|| AgentEvent::TextDelta {
            text: concat!(
                "\n### Code check\n\n",
                "The `fold_event_into_parts` helper feeds `writer.sync` on a `120ms` cadence:\n\n",
                "```rust\n",
                "// Fold one event into the accumulated parts.\n",
                "pub fn fold(mut acc: Vec<Part>, event: &AgentEvent) -> Vec<Part> {\n",
                "    let label = \"delta\";\n",
                "    if acc.len() > 128 {\n",
                "        acc.truncate(64); // keep the tail hot\n",
                "    }\n",
                "    acc\n",
                "}\n",
                "```\n\n",
                "```ts\n",
                "// Subscribe and fold on the client.\n",
                "const room = await connect(\"wss://mesh.local\", { retries: 3 });\n",
                "export function fold(parts: Part[], event: AgentEvent): Part[] {\n",
                "    return event.kind === \"delta\" ? [...parts, event] : parts;\n",
                "}\n",
                "```\n\n",
            )
            .into(),
        });
        let table_event = mock_table.then(|| AgentEvent::TextDelta {
            text: "\n### Table check\n\n\
                | Column A | Column B | Column C |\n\
                |---|---|---|\n\
                | a1 | b1 | c1 |\n\
                | a2 | b2 | c2 |\n\n\
                And a wide, uneven one:\n\n\
                | Stage | What happens | p95 |\n\
                |:--|:--|--:|\n\
                | Fold | Events fold into parts and diff into the Loro doc on a 120ms coalesced commit cadence, keeping the oplog RLE-merged across devices | 4.2ms |\n\
                | Sync | Session-room fan-out | 18ms |\n\n"
                .into(),
        });
        // Dev/testing knob: `KOMET_MOCK_MEND=1` appends a link/list-heavy
        // passage — bold-led list items, inline links, emphasis, strikethrough
        // — the shapes whose half-streamed markers the display mend
        // (crates/ui markdown/mend.rs) must hold steady while streaming.
        let mock_mend = std::env::var("KOMET_MOCK_MEND")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let mend_event = mock_mend.then(|| AgentEvent::TextDelta {
            text: concat!(
                "\n### Streaming mend check\n\n",
                "Inline styles hold while text arrives: **bold stays bold**, ",
                "*italic stays italic*, `code stays code`, and ~~this stays struck~~.\n\n",
                "- **Fold** — parts diff into the [Loro doc](https://loro.dev) on a 120ms cadence\n",
                "- **Relay** — commits fan out through the [session room](https://developers.cloudflare.com/durable-objects/) to every device\n",
                "- **Paint** — the [display tree](https://github.com/pulldown-cmark/pulldown-cmark) mends hanging markers in the last block only\n\n",
                "Links above never flash their URLs, and closing markers never reflow the paragraph.\n",
            )
            .into(),
        });
        // Dev/testing knob: `KOMET_MOCK_SUBAGENT=1` appends two spawn chips
        // whose nested traffic arrives as tagged `AgentEvent::Subagent`
        // events — the only data-side way to put spawn chips (running → done)
        // AND their openable subagent docs on screen with the mock harness.
        // The second subagent finishes after a beat of nested activity, so a
        // paced run (`KOMET_MOCK_DELAY_MS`) holds a Running chip long enough
        // to observe.
        let mock_subagent = std::env::var("KOMET_MOCK_SUBAGENT")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let subagent_events = mock_subagent
            .then(|| {
                let tag = |parent: &str, event: AgentEvent| AgentEvent::Subagent {
                    parent_tool_use_id: parent.into(),
                    event: Box::new(event),
                };
                let spawn = |id: &str, description: &str, prompt: &str| AgentEvent::ToolCall {
                    id: id.into(),
                    // The claude-driver spawn shape: `Agent: {description}`
                    // with the task in the input (names the chip AND the tab).
                    call: komet_proto::ToolCall::Unknown {
                        name: format!("Agent: {description}"),
                        input: Some(serde_json::json!({
                            "description": description,
                            "prompt": prompt,
                        })),
                    },
                };
                let resolve = |id: &str| AgentEvent::ToolResult {
                    id: id.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                };
                let done = AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
reason: None,
                };
                vec![
                    AgentEvent::TextDelta {
                        text: "\n### Subagent check\n\nFanning out two scouts before the fold rewrite.\n\n".into(),
                    },
                    spawn(
                        "mock-sub-1",
                        "Audit the fold path",
                        "Read crates/doc and list every call site of fold_event_into_parts, checking each holds the byte cap.",
                    ),
                    spawn(
                        "mock-sub-2",
                        "Verify the commit cadence",
                        "Measure the 120ms coalesced commit cadence under a scripted delta burst.",
                    ),
                    // The spawn prompts seed each subagent's opening user
                    // entry (like the claude driver's Task-prompt seeding).
                    tag(
                        "mock-sub-1",
                        AgentEvent::UserMessage {
                            text: "Read crates/doc and list every call site of fold_event_into_parts, checking each holds the byte cap.".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::UserMessage {
                            text: "Measure the 120ms coalesced commit cadence under a scripted delta burst.".into(),
                        },
                    ),
                    tag(
                        "mock-sub-1",
                        AgentEvent::TextDelta {
                            text: "Scanning `crates/doc` for fold call sites.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-sub-1",
                        AgentEvent::ToolCall {
                            id: "sub1-grep".into(),
                            call: komet_proto::ToolCall::Exec {
                                command: "grep -rn fold_event_into_parts crates".into(),
                            },
                        },
                    ),
                    tag("mock-sub-1", resolve("sub1-grep")),
                    tag(
                        "mock-sub-1",
                        AgentEvent::TextDelta {
                            text: "Three call sites: the live fold, the rebuild, and the subagent sink — every one applies the byte cap before persisting.".into(),
                        },
                    ),
                    resolve("mock-sub-1"),
                    tag("mock-sub-1", done.clone()),
                    // A steer AFTER the subagent settled (claude's queued
                    // SendMessage shape): resurrects the chip and reopens
                    // the frozen transcript for the resumed segment.
                    tag(
                        "mock-sub-1",
                        AgentEvent::UserMessage {
                            text: "One more: confirm the rebuild path holds the byte cap too."
                                .into(),
                        },
                    ),
                    tag(
                        "mock-sub-1",
                        AgentEvent::TextDelta {
                            text: "Rebuild path checked — same cap, applied before persisting."
                                .into(),
                        },
                    ),
                    tag("mock-sub-1", done.clone()),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Driving a 2k-delta burst through the writer.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::ToolCall {
                            id: "sub2-burst".into(),
                            call: komet_proto::ToolCall::Exec {
                                command: "cargo test -p komet-doc cadence_burst -- --nocapture".into(),
                            },
                        },
                    ),
                    resolve("mock-sub-2"),
                    tag("mock-sub-2", resolve("sub2-burst")),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Commits land on the 120ms cadence; no commit carried more than one burst.".into(),
                        },
                    ),
                    // A parent→subagent steer: splits the transcript into a
                    // user entry + fresh assistant segment (like the claude
                    // driver's tagged user text blocks).
                    tag(
                        "mock-sub-2",
                        AgentEvent::UserMessage {
                            text: "Also verify the cadence holds while a steer lands mid-burst.".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Re-running with a mid-burst steer injected.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::ToolCall {
                            id: "sub2-steer-burst".into(),
                            call: komet_proto::ToolCall::Exec {
                                command: "cargo test -p komet-doc cadence_steer -- --nocapture"
                                    .into(),
                            },
                        },
                    ),
                    tag("mock-sub-2", resolve("sub2-steer-burst")),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Watching the commit log while the burst drains: ".into(),
                        },
                    ),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 1 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 2 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 3 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 4 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 5 clean — ".into() }),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "every window under 120ms.\n\nSteer landed between commits; the cadence held.".into(),
                        },
                    ),
                    tag("mock-sub-2", done),
                ]
            })
            .into_iter()
            .flatten();
        // With the code knob, also exercise a MULTILINE Exec command — the
        // round-9 chip breaker shape ("set -e\nfixture_in_original=0"): the
        // Run chip must stay one 30px line.
        let code_tool_events = mock_code
            .then(|| {
                [
                    AgentEvent::ToolCall {
                        id: "mock-code-tool".into(),
                        call: komet_proto::ToolCall::Exec {
                            command: "set -e\nfixture_in_original=0\ngrep -rn \"veil\" crates/ui/src | wc -l".into(),
                        },
                    },
                    AgentEvent::ToolResult {
                        id: "mock-code-tool".into(),
                        is_error: false,
                        output: None,
                        diff: None,
                    },
                ]
            })
            .into_iter()
            .flatten();
        let events: Vec<Result<AgentEvent, HarnessError>> = body
            .iter()
            .cycle()
            .take(body.len() * repeat)
            .cloned()
            .chain(code_tool_events)
            .chain(subagent_events)
            .chain(code_event)
            .chain(table_event)
            .chain(mend_event)
            .chain(error_event)
            .chain(tail.iter().cloned())
            .map(Ok)
            .collect();
        // Dev/testing knob: `KOMET_MOCK_CHARS=N` re-chunks every TextDelta
        // into N-char deltas, so `KOMET_MOCK_DELAY_MS` paces *characters*
        // instead of whole scripted blocks — delta boundaries then land inside
        // inline markers and links, which is the streaming shape real
        // harnesses produce and the display mend exists for.
        let chunk_chars = std::env::var("KOMET_MOCK_CHARS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0);
        let events: Vec<Result<AgentEvent, HarnessError>> = match chunk_chars {
            None => events,
            Some(n) => events
                .into_iter()
                .flat_map(|event| match event {
                    Ok(AgentEvent::TextDelta { text }) => {
                        let chars: Vec<char> = text.chars().collect();
                        chars
                            .chunks(n)
                            .map(|c| {
                                Ok(AgentEvent::TextDelta {
                                    text: c.iter().collect(),
                                })
                            })
                            .collect::<Vec<_>>()
                    }
                    other => vec![other],
                })
                .collect(),
        };
        // Dev/testing knob: `KOMET_MOCK_SUBAGENT_DELAY_MS` paces TAGGED
        // (subagent) events on their own clock — the parent turn settles at
        // `KOMET_MOCK_DELAY_MS` speed while the background subagents stream
        // on slowly, which is exactly the eager-done shape live tabs are
        // observed under (and the only way a rig click can reliably land
        // inside a subagent's streaming window).
        let sub_delay_ms = std::env::var("KOMET_MOCK_SUBAGENT_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        if delay_ms == 0 && sub_delay_ms.is_none() {
            return Ok(futures::stream::iter(events).boxed());
        }
        Ok(futures::stream::iter(events)
            .then(move |event| async move {
                let pause = match (&event, sub_delay_ms) {
                    (Ok(AgentEvent::Subagent { .. }), Some(ms)) => {
                        std::time::Duration::from_millis(ms)
                    }
                    _ => delay,
                };
                tokio::time::sleep(pause).await;
                event
            })
            .boxed())
    }
}

/// Scripted compaction scenario (`KOMET_MOCK_SCENARIO=compaction`): two cycles
/// in one run — one that finishes with a recap, one that fails — so the thread
/// row, the recap fetch and the `Failed` state are all reachable without a live
/// agent. Ids are FIXED: the fold refines a cycle by id, so a scenario that
/// rotated its id per call would not replay as one row.
pub fn compaction_script() -> Vec<AgentEvent> {
    let compaction = |id: &str,
                      status: komet_proto::CompactionStatus,
                      trigger: Option<&str>,
                      tokens_before: Option<u64>,
                      summary: Option<&str>,
                      error: Option<&str>| AgentEvent::ContextCompaction {
        id: id.into(),
        status,
        trigger: trigger.map(str::to_owned),
        tokens_before,
        tokens_after: None,
        summary: summary.map(str::to_owned),
        summary_delta: None,
        error: error.map(str::to_owned),
    };
    use komet_proto::CompactionStatus;
    vec![
        AgentEvent::TextDelta {
            text: "Reconciling the ledger across the last forty turns…\n\n".into(),
        },
        compaction(
            "mock-cmp-1",
            CompactionStatus::InProgress,
            None,
            None,
            None,
            None,
        ),
        compaction(
            "mock-cmp-1",
            CompactionStatus::Completed,
            Some("auto"),
            Some(165_000),
            Some(
                "User asked for the reconciliation path. Four files were read, the sync design was \
                 agreed, and the failing suite was identified.",
            ),
            None,
        ),
        AgentEvent::TextDelta {
            text: "Context rebuilt — continuing from the recap.\n\n".into(),
        },
        compaction(
            "mock-cmp-2",
            CompactionStatus::InProgress,
            Some("manual"),
            None,
            None,
            None,
        ),
        compaction(
            "mock-cmp-2",
            CompactionStatus::Failed,
            Some("manual"),
            None,
            None,
            Some("The summarizer timed out; the conversation is unchanged."),
        ),
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            reason: None,
        },
    ]
}

/// Scripted nested-subagent scenario (`KOMET_MOCK_SCENARIO=subagent_nested`):
/// A spawns B, and both stream tools and text. Each child event is wrapped in
/// its DIRECT parent's `Subagent` — the wire's own shape (a sub-subagent tags
/// its spawner, not the root) — so the tree is something Task 4 derives from the
/// spawn records rather than something the mock pre-bakes.
pub fn subagent_nested_script() -> Vec<AgentEvent> {
    let child = |parent: &str, event: AgentEvent| AgentEvent::Subagent {
        parent_tool_use_id: parent.into(),
        event: Box::new(event),
    };
    let agent = |id: &str, label: &str| AgentEvent::ToolCall {
        id: id.into(),
        call: komet_proto::ToolCall::Unknown {
            name: format!("Agent: {label}"),
            input: None,
        },
    };
    vec![
        AgentEvent::TextDelta {
            text: "Two levels of delegation: an investigator, and a reader under it.\n\n".into(),
        },
        agent("mock-spawn-a", "investigate the fold"),
        child(
            "mock-spawn-a",
            AgentEvent::TextDelta {
                text: "I'll delegate the file read.\n\n".into(),
            },
        ),
        child("mock-spawn-a", agent("mock-spawn-b", "read sessions.rs")),
        child(
            "mock-spawn-b",
            AgentEvent::ToolCall {
                id: "mock-bash-b".into(),
                call: komet_proto::ToolCall::Exec {
                    command: "sed -n 1,120p crates/engine/src/sessions.rs".into(),
                },
            },
        ),
        child(
            "mock-spawn-b",
            AgentEvent::ToolResult {
                id: "mock-bash-b".into(),
                is_error: false,
                output: None,
                diff: None,
            },
        ),
        child(
            "mock-spawn-b",
            AgentEvent::TextDelta {
                text: "The fold keys each part by id, so a cycle refines in place.\n\n".into(),
            },
        ),
        child(
            "mock-spawn-b",
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
                reason: None,
            },
        ),
        child(
            "mock-spawn-a",
            AgentEvent::ToolResult {
                id: "mock-spawn-b".into(),
                is_error: false,
                output: Some("The fold keys each part by id.".into()),
                diff: None,
            },
        ),
        child(
            "mock-spawn-a",
            AgentEvent::TextDelta {
                text: "Root cause: the row fingerprint missed the new fields.\n\n".into(),
            },
        ),
        child(
            "mock-spawn-a",
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
                reason: None,
            },
        ),
        AgentEvent::ToolResult {
            id: "mock-spawn-a".into(),
            is_error: false,
            output: Some("Root cause: the row fingerprint missed the new fields.".into()),
            diff: None,
        },
        AgentEvent::TextDelta {
            text: "Both levels reported in.".into(),
        },
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            reason: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::CompactionStatus;

    /// The `question_multi` demo scenario is what `dev-demo.sh --questions`
    /// and the UI tests drive: two pages, one multi-select, free text on both.
    #[test]
    fn mock_question_multi_scenario_has_pages_checks_and_free_text() {
        let questions = question_script();
        assert_eq!(questions.len(), 2, "two questions, one per page");
        assert!(
            !questions[0].multi_select,
            "the first page is a single pick"
        );
        assert!(
            questions[1].multi_select,
            "the second page takes several answers"
        );
        assert!(
            questions.iter().all(|q| q.allow_free_text),
            "each page offers the free-text ('Autre') field"
        );
        assert!(questions.iter().all(|q| !q.options.is_empty()));
    }

    /// A scripted scenario must replay identically: the fold refines a
    /// compaction by id, so a rotating id (or a clock) would turn one boundary
    /// into two rows — and make every test that reads the script flaky.
    #[test]
    fn mock_compaction_scenario_is_deterministic() {
        assert_eq!(compaction_script(), compaction_script());
        let cycles: Vec<_> = compaction_script()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ContextCompaction { id, status, .. } => Some((id.clone(), *status)),
                _ => None,
            })
            .collect();
        assert_eq!(
            cycles,
            vec![
                ("mock-cmp-1".to_owned(), CompactionStatus::InProgress),
                ("mock-cmp-1".to_owned(), CompactionStatus::Completed),
                ("mock-cmp-2".to_owned(), CompactionStatus::InProgress),
                ("mock-cmp-2".to_owned(), CompactionStatus::Failed),
            ]
        );
        // The recap belongs to the settled cycle and the failure carries its
        // reason: without them the row has nothing honest to expand to.
        let settled = compaction_script().into_iter().find_map(|e| match e {
            AgentEvent::ContextCompaction {
                id,
                summary: Some(text),
                ..
            } if id == "mock-cmp-1" => Some(text),
            _ => None,
        });
        assert!(settled.is_some_and(|text| !text.trim().is_empty()));
        let failed = compaction_script().into_iter().find_map(|e| match e {
            AgentEvent::ContextCompaction {
                id,
                error: Some(text),
                ..
            } if id == "mock-cmp-2" => Some(text),
            _ => None,
        });
        assert!(failed.is_some_and(|text| !text.trim().is_empty()));
        assert!(
            compaction_script().iter().any(|e| matches!(
                e,
                AgentEvent::ContextCompaction {
                    tokens_before: Some(165_000),
                    ..
                }
            )),
            "the row needs a before-size to render its arrow"
        );
        assert_eq!(
            compaction_script()
                .iter()
                .filter(|e| matches!(e, AgentEvent::Done { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn mock_subagent_nested_scenario_is_deterministic() {
        assert_eq!(subagent_nested_script(), subagent_nested_script());
        let parents: Vec<String> = ["mock-spawn-a", "mock-spawn-b"]
            .iter()
            .map(|id| id.to_string())
            .collect();
        for event in &subagent_nested_script() {
            if let AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } = event
            {
                assert!(parents.contains(parent_tool_use_id), "unknown parent");
                // Exactly one hop per event: the tree is derived from the spawn
                // records, not pre-baked as double-wrapped traffic.
                assert!(!matches!(**event, AgentEvent::Subagent { .. }));
            }
        }
        // Both levels stream tools AND text on their own feed, and each settles
        // with its own Done under its direct parent.
        for parent in &parents {
            let stream = subagent_nested_script();
            let mine = stream.iter().filter(|e| {
                matches!(
                    e,
                    AgentEvent::Subagent { parent_tool_use_id, .. } if parent_tool_use_id == parent
                )
            });
            let text = mine
                .clone()
                .filter(|e| {
                    matches!(
                        e,
                        AgentEvent::Subagent { event, .. } if matches!(**event, AgentEvent::TextDelta { .. })
                    )
                })
                .count();
            let tools = mine
                .clone()
                .filter(|e| {
                    matches!(
                        e,
                        AgentEvent::Subagent { event, .. } if matches!(**event, AgentEvent::ToolCall { .. })
                    )
                })
                .count();
            let settled = mine
                .filter(|e| {
                    matches!(
                        e,
                        AgentEvent::Subagent { event, .. } if matches!(**event, AgentEvent::Done { .. })
                    )
                })
                .count();
            assert!(
                text >= 1 && tools >= 1 && settled == 1,
                "{parent}: {text} text, {tools} tools, {settled} done"
            );
        }
        // The spawn chips themselves ride the feed that created them: A on the
        // parent's, B on A's.
        let spawn_b_on_a_feed = subagent_nested_script().into_iter().any(|e| {
            let AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } = e
            else {
                return false;
            };
            parent_tool_use_id == "mock-spawn-a"
                && matches!(&*event, AgentEvent::ToolCall { id, .. }
                    if id == "mock-spawn-b")
        });
        assert!(
            spawn_b_on_a_feed,
            "B must be spawned from A's feed, not the root's"
        );
    }
}
