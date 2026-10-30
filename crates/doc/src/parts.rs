//! Message parts: the event fold, the render-only privacy policy, and continuation splitting.
//!
//! Ports of `packages/control/src/parts.ts` (fold) and
//! `packages/session-doc/src/{render-parts,messages}.ts`.

use serde::{Deserialize, Serialize};

use komet_proto::{
    AgentEvent, CompactionStatus, ErrorKind, QuestionSource, SUBAGENT_MODEL_KEYS, ToolCall,
    ToolDiff, UserInputAnswer, UserInputQuestion,
};

use crate::constants::{COMPACTION_SUMMARY_MAX_BYTES, CompactionSummaryPolicy, MSG_INLINE_MAX};

/// Char cap for the tool-output SUMMARY persisted into the doc: first
/// non-empty line, nothing more. The per-part 4KB cap (c951c3e) bounded each
/// part but not the session — chat 1b65e93d measured 917KB (85%) of a 1MB doc
/// in capped outputs across 426 tool parts (docs/chat2-sync.md). Full outputs
/// live in the R2 sidecar behind `output_ref`; t3code ships an 84-char
/// summary, so 160 is generous.
pub const TOOL_OUTPUT_SUMMARY_MAX: usize = 160;

/// The doc-resident form of a tool output (docs/chat2-sync.md A1; the R2
/// sidecar is PARKED as of 2026-08-10, so this IS the whole record in the
/// doc — the full text survives only in the host's local run journal):
///
/// - Markdown code fences are stripped first — ACP harnesses fence every
///   output, so the fence is transport wrapping, never content (pre-fix,
///   every summary read "```console…").
/// - Outputs that fit [`TOOL_OUTPUT_SUMMARY_MAX`] chars ride whole — a
///   summary of a two-line output is more UI than the output.
/// - Bigger outputs keep the first non-empty line, capped, with a `…`
///   marker meaning "there was more".
///
/// `None` for blank output.
pub fn summarize_tool_output(text: &str) -> Option<String> {
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    let stripped = kept.join("\n");
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return None;
    }
    if stripped.chars().count() <= TOOL_OUTPUT_SUMMARY_MAX {
        return Some(stripped.to_owned());
    }
    // Too big to inline: first non-empty line, capped. There is always more
    // than the summary here (whole output exceeded the budget), so the
    // marker is unconditional.
    let line = stripped
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(stripped)
        .trim_end();
    let mut end = line.len();
    for (chars, (i, _)) in line.char_indices().enumerate() {
        if chars == TOOL_OUTPUT_SUMMARY_MAX {
            end = i;
            break;
        }
    }
    let mut out = line[..end].to_owned();
    out.push('…');
    Some(out)
}

/// Per-file diff stats persisted in place of inline diff text (t3's shape).
/// The inline diff was the bigger bomb than outputs — 32KB/edit, unexercised
/// only because the claude harness emits none. Full diff text lives in the
/// sidecar behind `diff_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiffStat {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

/// Line-level add/delete counts for one file's diff.
pub fn diff_stat(diff: &ToolDiff) -> ToolDiffStat {
    let (additions, deletions) = match &diff.old_text {
        None => (diff.new_text.lines().count() as u64, 0),
        Some(old) => {
            let text_diff = similar::TextDiff::from_lines(old.as_str(), diff.new_text.as_str());
            let mut additions = 0u64;
            let mut deletions = 0u64;
            for change in text_diff.iter_all_changes() {
                match change.tag() {
                    similar::ChangeTag::Insert => additions += 1,
                    similar::ChangeTag::Delete => deletions += 1,
                    similar::ChangeTag::Equal => {}
                }
            }
            (additions, deletions)
        }
    };
    ToolDiffStat {
        path: diff.path.clone(),
        additions,
        deletions,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageStatus {
    Streaming,
    Complete,
    Aborted,
}

/// Lifecycle of a spawned subagent, carried on its spawn chip. Ported
/// alongside the fold logic below — Komet had no equivalent before this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubagentStatus {
    Running,
    Done,
    Failed,
}

/// One answer recorded against a question, written by the device that answered
/// it. Append-only: a question two devices answered offline carries both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputResponse {
    pub answers: Vec<UserInputAnswer>,
    /// Epoch millis on the answering device's clock. Display only — device
    /// clocks are not comparable, so nothing here is ever ordered or chosen by
    /// it (the winner is doc position, see [`MessagePart::Input::responses`]).
    pub decided_at: i64,
    /// The device that answered (its `issued_by`), so a second device can tell
    /// whose answer the agent got.
    pub device: String,
    /// The answer the host actually delivered to the agent. Set by the host at
    /// delivery; never derived from the position in `responses`.
    #[serde(default)]
    pub applied: bool,
}

/// One rendered part of an assistant message.
///
/// Kept as a struct-variant enum (not a struct) for t3/Komet serde
/// compatibility: the JSON shape is `{"kind": "tool", ...}` per variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum MessagePart {
    Text {
        id: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        call: ToolCall,
        #[serde(default)]
        is_error: bool,
        /// True once a ToolResult arrived.
        #[serde(default)]
        resolved: bool,
        /// One-line tool output summary ([`summarize_tool_output`]). Old
        /// entries (pre-strip) still carry up to 4KB here; old app versions
        /// render this field either way, so the strip is invisible to them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Inline file diff — written by pre-strip app versions only; new
        /// folds persist [`Self::Tool::diff_stats`] + `diff_ref` instead.
        /// Kept so old docs render their diffs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<ToolDiff>,
        /// Sidecar key (`{chatId}/{partId}`) of the full output — additive;
        /// stamped by [`apply_sidecar_refs`] (the fold is chat-agnostic).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_ref: Option<String>,
        /// Full-output byte length, so the UI can say "Show full output (12 KB)".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_bytes: Option<u64>,
        /// Sidecar key (`{chatId}/{partId}.diff`) of the full diff JSON.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_ref: Option<String>,
        /// Per-file diff stats (additive replacement for inline `diff`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_stats: Option<Vec<ToolDiffStat>>,
        /// Reserved in the schema alongside `subagent_status`/`subagent_tail`
        /// (Komet's upstream shape) — the fold below matches a spawn chip by
        /// its own `id == parent_tool_use_id` plus `call.is_subagent_spawn()`,
        /// so this field isn't read by that path; kept for schema parity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_ref: Option<String>,
        /// Current lifecycle of the spawned subagent (see [`SubagentStatus`]).
        /// `None` until the first lifecycle-bearing event arrives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_status: Option<SubagentStatus>,
        /// Reserved for a live text tail on the spawn chip — tried and
        /// rejected upstream (rewriting the chip per delta batch bloated the
        /// parent doc's oplog for the whole subagent run and read as
        /// distracting mid-stream fragments), kept in the schema so docs
        /// that carry it from elsewhere still deserialize.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_tail: Option<String>,
        /// Wall-clock length of the subagent's last cycle, stamped on the chip
        /// by the host's run loop at the child's terminal event — the one
        /// terminal write, so a long run costs the doc nothing per frame.
        /// `None` means "not measured on this device": a chip whose run died
        /// before its terminal event (a restart has no timestamps in the
        /// journal to reconstruct it from), or a chip written by an older
        /// build. Never `Some(0)` for unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_duration_ms: Option<u64>,
        /// How many tools the subagent ran, stamped with the same terminal
        /// write as [`Self::subagent_duration_ms`]. Live counting does NOT go
        /// through the doc: it rides `WatchActivity`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_tool_count: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Input {
        id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default)]
        resolved: bool,
        /// Every answer the question received, in doc order — append-only, so
        /// two devices answering offline both land here and the merge stays
        /// deterministic. Position 0 is the display winner; `applied` marks the
        /// answer the host actually handed to the agent, which is not always
        /// position 0. Absent on a doc written before answers were kept: an
        /// empty list means "no answer recorded", never "no answer given".
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        responses: Vec<InputResponse>,
        /// Which wire the question came in on, so the transcript can say
        /// whether the answer went back to the agent's own tool or to an
        /// approval-style prompt.
        #[serde(default, skip_serializing_if = "QuestionSource::is_native")]
        source: QuestionSource,
    },
    #[serde(rename_all = "camelCase")]
    Switch {
        id: String,
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_harness: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_harness: Option<String>,
    },
    Error {
        id: String,
        message: String,
        /// Typed class of the failure; `Unknown` on old rows. Renamed on the
        /// wire: the enum's internal serde tag already occupies `kind`.
        #[serde(
            default,
            skip_serializing_if = "ErrorKind::is_unknown",
            rename = "errorKind"
        )]
        kind: ErrorKind,
        /// Raw CLI text / stderr tail backing the Details view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// Whether the UI offers a retry action.
        #[serde(default)]
        retryable: bool,
    },
    #[serde(rename_all = "camelCase")]
    Permission {
        id: String,
        request_id: String,
        permission_kind: komet_proto::PermissionKind,
        summary: String,
        choices: Vec<komet_proto::PermissionChoice>,
        /// Provider-native dynamic actions (Paseo `AgentPermissionAction[]`);
        /// empty → UI falls back to the fixed Deny/Allow(/AllowAlways) set.
        #[serde(default)]
        actions: Vec<komet_proto::AgentPermissionAction>,
        #[serde(default)]
        resolved: bool,
        decision: Option<komet_proto::PermissionChoice>,
        /// The tool call this approval gates, when the agent named one. Lets
        /// the transcript point at the chip the decision un-blocked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Which device decided, and when (epoch millis of that device's
        /// clock, display only). `None` on a decision recorded before devices
        /// were kept — the decision itself is not invented from it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decided_by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decided_at: Option<i64>,
        /// Epoch-ms deadline of the host watchdog that auto-denies this
        /// request. `None` on docs written before the field existed and when
        /// no watchdog is armed. Display-only: the engine owns the decision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<i64>,
    },
    /// One context-compaction boundary in the history: the agent squeezed
    /// everything before this point into a summary. One part per compaction
    /// cycle — `compaction_id` is the agent's identity for the cycle (or the
    /// adapter's synthesized one) and `id` is the doc key derived from it, so
    /// re-folding the same cycle updates this part instead of stacking another.
    #[serde(rename_all = "camelCase")]
    Compaction {
        id: String,
        compaction_id: String,
        status: komet_proto::CompactionStatus,
        /// Why ("auto" / "manual" / the provider's own wording); `None` when
        /// the agent did not say — the UI shows no reason rather than a guess.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        /// `None` = unknown (see [`Self::summary_bytes`] for the same
        /// no-invention rule): the UI draws no arrow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
        /// Why it failed, on `Failed` — including the engine's own verdict for
        /// a compaction whose run died mid-flight.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Summary TEXT, present only under
        /// [`CompactionSummaryPolicy::SummaryInDoc`] and then capped to
        /// [`COMPACTION_SUMMARY_MAX_BYTES`]. Default docs carry no text: the
        /// host's run journal does, fetched through `GetCompactionSummary`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Byte length of the summary as the agent produced it — known even
        /// when the text never entered the doc, so the UI can say how much
        /// there is to fetch (and how much a cap dropped).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_bytes: Option<u64>,
        /// A complete summary exists in the HOST's run journal. Synced as
        /// written by the host; a viewer that cannot read that journal still
        /// gets a typed "unavailable", not an empty string.
        #[serde(default)]
        summary_in_journal: bool,
    },
}

impl MessagePart {
    pub fn id(&self) -> &str {
        match self {
            MessagePart::Text { id, .. }
            | MessagePart::Tool { id, .. }
            | MessagePart::Input { id, .. }
            | MessagePart::Switch { id, .. }
            | MessagePart::Error { id, .. }
            | MessagePart::Permission { id, .. }
            | MessagePart::Compaction { id, .. } => id,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            MessagePart::Text { text, .. } => text.len(),
            MessagePart::Tool {
                call,
                output,
                diff,
                diff_stats,
                ..
            } => {
                serde_json::to_vec(call).map_or(0, |v| v.len())
                    + output.as_ref().map_or(0, String::len)
                    + diff
                        .as_ref()
                        .map_or(0, |d| serde_json::to_vec(d).map_or(0, |v| v.len()))
                    + diff_stats
                        .as_ref()
                        .map_or(0, |s| serde_json::to_vec(s).map_or(0, |v| v.len()))
            }
            MessagePart::Input { questions, .. } => {
                serde_json::to_vec(questions).map_or(0, |v| v.len())
            }
            MessagePart::Switch {
                from,
                to,
                from_harness,
                to_harness,
                ..
            } => {
                from.len()
                    + to.len()
                    + from_harness.as_ref().map_or(0, String::len)
                    + to_harness.as_ref().map_or(0, String::len)
            }
            MessagePart::Error { message, .. } => message.len(),
            MessagePart::Permission { summary, .. } => summary.len(),
            MessagePart::Compaction {
                summary, trigger, ..
            } => summary.as_ref().map_or(0, String::len) + trigger.as_ref().map_or(0, String::len),
        }
    }
}

/// Fold one agent event into a parts accumulator, in place.
///
/// In place because the fold runs once per streamed event: rebuilding the
/// accumulator each time made long turns O(n²) in allocations.
///
/// Semantics from komet `foldEventIntoParts`:
/// - `SessionStarted` / `Steered` reset the accumulator (turn boundary — makes replay safe).
/// - `TextDelta` appends to the trailing text part, or starts a new one if the trail is not text
///   (a tool call in between breaks the text block).
/// - `ToolCall` appends, or refreshes in place when the id already exists (SDK retry idempotence).
/// - `ToolResult` marks the matching tool part resolved / errored in place.
/// - `InputRequested` appends an input part; `InputResolved` marks it resolved.
/// - `Error` and `Done{error}` become visible error parts.
/// - `ContextCompaction` updates one part per compaction id (see
///   [`fold_compaction`]).
///
/// Summary text is never carried into the doc by this form: it takes the
/// default [`CompactionSummaryPolicy::JournalOnly`]. A caller with a setting to
/// consult uses [`fold_event_into_parts_with`].
pub fn fold_event_into_parts(out: &mut Vec<MessagePart>, event: &AgentEvent) {
    fold_event_into_parts_with(out, event, CompactionSummaryPolicy::default());
}

/// Fold one agent event into a parts accumulator, with an explicit compaction
/// summary policy.
pub fn fold_event_into_parts_with(
    out: &mut Vec<MessagePart>,
    event: &AgentEvent,
    summary_policy: CompactionSummaryPolicy,
) {
    match event {
        AgentEvent::SessionStarted { .. } | AgentEvent::Steered { .. } => {
            out.clear();
        }
        AgentEvent::TextDelta { text } => {
            if let Some(MessagePart::Text { text: tail, .. }) = out.last_mut() {
                tail.push_str(text);
            } else {
                let id = format!("t{}", out.len());
                out.push(MessagePart::Text {
                    id,
                    text: text.clone(),
                });
            }
        }
        AgentEvent::ReasoningDelta { .. } => {
            // Reasoning is not rendered as a transcript part (matches komet).
        }
        AgentEvent::ToolCall { id, call } => {
            if let Some(existing) = out.iter_mut().find_map(|p| match p {
                MessagePart::Tool {
                    id: pid, call: c, ..
                } if pid == id => Some(c),
                _ => None,
            }) {
                // Never downgrade a typed call to Unknown: completion updates
                // often carry title-only shape (no kind/rawInput), which would
                // clobber e.g. a Todo chip into a JSON dump. Unknown existing
                // still refreshes (titles carry live progress like "3 todos").
                let incoming_unknown = matches!(call, ToolCall::Unknown { .. });
                let existing_known = !matches!(existing, ToolCall::Unknown { .. });
                if !(incoming_unknown && existing_known) {
                    *existing = call.clone();
                }
            } else {
                out.push(MessagePart::Tool {
                    id: id.clone(),
                    call: call.clone(),
                    is_error: false,
                    resolved: false,
                    output: None,
                    diff: None,
                    output_ref: None,
                    output_bytes: None,
                    diff_ref: None,
                    diff_stats: None,
                    subagent_ref: None,
                    subagent_status: None,
                    subagent_tail: None,
                    subagent_duration_ms: None,
                    subagent_tool_count: None,
                });
            }
        }
        AgentEvent::ToolResult {
            id,
            is_error,
            output,
            diff,
        } => {
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id: pid,
                    is_error: e,
                    resolved,
                    output: out_slot,
                    diff: diff_slot,
                    output_bytes,
                    diff_stats,
                    ..
                } = p
                    && pid == id
                {
                    *e = *is_error;
                    *resolved = true;
                    // Tool OUTPUTS never enter the doc (2026-08-10 product
                    // call: chips are one-liners — name + call info — like
                    // pre-output builds; the R2 sidecar is parked with them,
                    // docs/chat2-sync.md A2). Full text lives only in the
                    // host's run journal. Inline diffs die the same way:
                    // stats only, never text. `is_error` still folds so
                    // failed chips read as failed.
                    //
                    // Exception: a FAILED tool keeps a 160-char summary of
                    // its output ([`summarize_tool_output`]) so the failure
                    // is diagnosable from the transcript; success outputs
                    // stay journal-only even when the event carries them.
                    let _ = output; // journal-only
                    *out_slot = if *is_error {
                        output.as_deref().and_then(summarize_tool_output)
                    } else {
                        None
                    };
                    *output_bytes = None;
                    *diff_slot = None;
                    *diff_stats = diff.as_ref().map(|d| vec![diff_stat(d)]);
                }
            }
        }
        AgentEvent::InputRequested {
            request_id,
            questions,
            source,
        } => {
            let id = format!("in-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Input {
                    id,
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolved: false,
                    responses: Vec::new(),
                    source: *source,
                });
            }
        }
        // The answers ride the event for the run journal (what the agent was
        // given), but they are NOT written from here: the answer's device and
        // clock come from the command that carried it, and the doc writer
        // (`SessionDoc::record_input_response`) is the one place `responses`
        // grows. A fold that appended here would write the same answer twice
        // — once by the answering device, once by whoever replayed the event.
        AgentEvent::InputResolved { request_id, .. } => {
            for p in out.iter_mut() {
                if let MessagePart::Input {
                    request_id: rid,
                    resolved,
                    ..
                } = p
                    && rid == request_id
                {
                    *resolved = true;
                }
            }
        }
        AgentEvent::Error {
            message,
            kind,
            detail,
            retryable,
        } => {
            let id = format!("e{}", out.len());
            out.push(MessagePart::Error {
                id,
                message: message.clone(),
                kind: *kind,
                detail: detail.clone(),
                retryable: *retryable,
            });
        }
        AgentEvent::Done { error, .. } => {
            if let Some(message) = error {
                // Terminal failures are announced by a typed Error event just
                // before the Done (same message, kind + stderr detail); the
                // Done then adds nothing — skip it so the transcript shows a
                // single detailed part instead of two chips.
                let dup = matches!(out.last(), Some(MessagePart::Error { message: m, .. }) if m == message);
                if !dup {
                    let id = format!("e{}", out.len());
                    out.push(MessagePart::Error {
                        id,
                        message: message.clone(),
                        kind: ErrorKind::Unknown,
                        detail: None,
                        retryable: false,
                    });
                }
            }
        }
        // Subagent CONTENT is not folded into this doc at all: it lives only in
        // the host's run journal, which `RunJournal::subagent_events` projects
        // into a transcript on demand (there is no child doc — plan
        // 2026-10-27-agent-transparency Task 4). The parent doc keeps the spawn
        // chip, and this arm refreshes it LIFECYCLE ONLY — the counts and
        // duration arrive in the host's single terminal write
        // (`subagent_duration_ms`/`subagent_tool_count`), never from here.
        // Ported with the "subagent steers: durable across harnesses" lifecycle
        // — Komet had a no-op here before this (see `subagent_status` above).
        AgentEvent::Subagent {
            parent_tool_use_id,
            event,
        } => {
            let status = match event.as_ref() {
                AgentEvent::Done { status, .. } => Some(match status {
                    komet_proto::DoneStatus::Errored => SubagentStatus::Failed,
                    _ => SubagentStatus::Done,
                }),
                // A steer RESURRECTS a settled chip — it announces more work
                // (claude: a queued SendMessage relaunches the agent), so
                // this is the one event allowed past the no-regress guard.
                AgentEvent::UserMessage { .. } => Some(SubagentStatus::Running),
                _ => None,
            };
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id,
                    call,
                    subagent_status,
                    ..
                } = p
                    && id == parent_tool_use_id
                    // Genus gate: only a SPAWN call ever carries subagent
                    // lifecycle. A driver keying bug decorating an ordinary
                    // chip (e.g. a background shell settling through the
                    // subagent subtype) must not flip its status.
                    && call.is_subagent_spawn()
                {
                    match status {
                        Some(s) => *subagent_status = Some(s),
                        // Any tagged traffic proves the subagent is live;
                        // never regress a terminal state.
                        None if !matches!(
                            subagent_status,
                            Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
                        ) =>
                        {
                            *subagent_status = Some(SubagentStatus::Running);
                        }
                        None => {}
                    }
                }
            }
        }
        // AvailableCommands feeds the engine's per-harness command cache, not
        // the transcript. A subagent's own UserMessage has no doc home either —
        // with no child doc, it survives only as a frame of the journal
        // projection.
        AgentEvent::PermissionRequested {
            request_id,
            kind,
            summary,
            choices,
            actions,
            tool_call_id,
            expires_at,
            ..
        } => {
            let id = format!("perm-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Permission {
                    id: id.clone(),
                    request_id: request_id.clone(),
                    permission_kind: kind.clone(),
                    summary: summary.clone(),
                    choices: choices.clone(),
                    actions: actions.clone(),
                    resolved: false,
                    decision: None,
                    tool_call_id: tool_call_id.clone(),
                    decided_by: None,
                    decided_at: None,
                    expires_at: *expires_at,
                });
            }
        }
        AgentEvent::PermissionResolved { request_id, choice } => {
            for p in out.iter_mut() {
                if let MessagePart::Permission {
                    request_id: rid,
                    resolved,
                    decision,
                    ..
                } = p
                    && rid == request_id
                {
                    // Deny-wins on conflict: if already resolved to Deny, keep it
                    if *resolved && matches!(decision, Some(komet_proto::PermissionChoice::Deny)) {
                        continue;
                    }
                    if matches!(choice, komet_proto::PermissionChoice::Deny) || !*resolved {
                        *decision = Some(choice.clone());
                        *resolved = true;
                    }
                }
            }
        }
        AgentEvent::ContextCompaction {
            id,
            status,
            trigger,
            tokens_before,
            tokens_after,
            summary,
            summary_delta,
            error,
        } => fold_compaction(
            out,
            id,
            *status,
            trigger.as_ref(),
            *tokens_before,
            *tokens_after,
            summary.as_ref(),
            summary_delta.as_ref(),
            error.as_ref(),
            summary_policy,
        ),
        AgentEvent::AssistantMessageCompleted { .. }
        | AgentEvent::Usage { .. }
        | AgentEvent::ContextWindow { .. }
        | AgentEvent::AvailableCommands { .. }
        | AgentEvent::ToolProgress { .. }
        | AgentEvent::RunPhase { .. }
        | AgentEvent::UserMessage { .. } => {}
    }
}

/// Fold a compaction cycle into the accumulator: exactly one part per
/// `compaction_id`, updated in place, so `inProgress` → `completed` refines the
/// same boundary instead of appending a second one, and a session that compacts
/// twice keeps two rows.
///
/// A field the agent stops carrying is never re-invented: `trigger`, the token
/// counts and `error` are written when present and left alone otherwise.
#[allow(clippy::too_many_arguments)]
fn fold_compaction(
    out: &mut Vec<MessagePart>,
    compaction_id: &str,
    status: CompactionStatus,
    trigger: Option<&String>,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
    summary: Option<&String>,
    summary_delta: Option<&String>,
    error: Option<&String>,
    policy: CompactionSummaryPolicy,
) {
    let id = format!("cmp-{compaction_id}");
    if !out.iter().any(|p| p.id() == id) {
        out.push(MessagePart::Compaction {
            id: id.clone(),
            compaction_id: compaction_id.to_owned(),
            status,
            trigger: None,
            tokens_before: None,
            tokens_after: None,
            error: None,
            summary: None,
            summary_bytes: None,
            summary_in_journal: false,
        });
    }
    let Some(MessagePart::Compaction {
        status: here,
        trigger: trigger_here,
        tokens_before: before_here,
        tokens_after: after_here,
        error: error_here,
        summary: summary_here,
        summary_bytes,
        summary_in_journal,
        ..
    }) = out.iter_mut().find(|p| p.id() == id)
    else {
        return;
    };
    // A settled cycle is not reopened by a late `inProgress` frame — providers
    // reorder the boundary and its status under load, and a transcript that
    // flickers back to "compacting" after it finished is worse than a stale one.
    if !matches!(
        here,
        CompactionStatus::Completed | CompactionStatus::Failed | CompactionStatus::Cancelled
    ) || !matches!(status, CompactionStatus::InProgress)
    {
        *here = status;
    }
    if trigger.is_some() {
        *trigger_here = trigger.cloned();
    }
    if tokens_before.is_some() {
        *before_here = tokens_before;
    }
    if tokens_after.is_some() {
        *after_here = tokens_after;
    }
    if error.is_some() {
        *error_here = error.cloned();
    }
    // The size the agent actually produced, whatever the policy: `summary` is a
    // complete replacement (it resets what the deltas accumulated), a
    // `summary_delta` extends it.
    if let Some(text) = summary {
        *summary_bytes = Some(text.len() as u64);
    }
    if let Some(delta) = summary_delta {
        *summary_bytes = Some(summary_bytes.unwrap_or(0) + delta.len() as u64);
    }
    if policy.summary_in_doc() {
        if let Some(text) = summary {
            *summary_here = Some(cap_summary(text));
        }
        if let Some(delta) = summary_delta {
            let mut accumulated = summary_here.take().unwrap_or_default();
            accumulated.push_str(delta);
            *summary_here = Some(cap_summary(&accumulated));
        }
    }
    // "There is a summary to fetch" — the journal keeps the whole text even
    // when the doc was capped, and holds nothing at all when the agent sent no
    // summary. Never true on an empty one: the UI would offer a blank fetch.
    *summary_in_journal = summary_bytes.unwrap_or(0) > 0;
}

/// Cut a summary at the doc's byte budget, on a UTF-8 boundary (never mid
/// character — a torn multi-byte tail is a corrupt string, not a truncation).
fn cap_summary(text: &str) -> String {
    if text.len() <= COMPACTION_SUMMARY_MAX_BYTES {
        return text.to_owned();
    }
    let mut end = COMPACTION_SUMMARY_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Stamp sidecar keys onto resolved tool parts that have sidecar content.
///
/// Separate from the fold because the fold is chat-agnostic and pure; the
/// caller (who knows the chat id) runs this right after each fold step, before
/// the parts hit the doc. Idempotent. Key shape `{chatId}/{partId}` (+
/// `.diff`) matches the edge's `/blob/{chatId}/{partId}` route.
pub fn apply_sidecar_refs(chat_id: &str, parts: &mut [MessagePart]) {
    for part in parts.iter_mut() {
        if let MessagePart::Tool {
            id,
            resolved: true,
            output_ref,
            output_bytes,
            diff_ref,
            diff_stats,
            ..
        } = part
        {
            if output_ref.is_none() && output_bytes.is_some() {
                *output_ref = Some(format!("{chat_id}/{id}"));
            }
            if diff_ref.is_none() && diff_stats.is_some() {
                *diff_ref = Some(format!("{chat_id}/{id}.diff"));
            }
        }
    }
}

/// The chat's current plan: the latest `ToolCall::Todo` list in doc order.
///
/// Todo calls carry complete snapshots (not deltas), so the last one wins. An
/// empty list is a real "plan cleared" and comes back as `Some(vec![])`;
/// `None` means the chat never emitted a plan. Backward scan: the live plan
/// sits near the tail, and a long transcript must not cost a full pass on
/// every streaming frame.
pub fn latest_plan(
    entries: &[crate::schema::SessionMessageEntry],
) -> Option<Vec<komet_proto::TodoItem>> {
    entries.iter().rev().find_map(|entry| {
        entry.parts.iter().rev().find_map(|part| match part {
            MessagePart::Tool {
                call: ToolCall::Todo { items },
                ..
            } => Some(items.clone()),
            _ => None,
        })
    })
}

/// What a [`AgentEvent::ToolResult`] owes the sidecar: the full output text
/// and/or the full diff (as JSON), keyed by part id. `None` when the event
/// carries nothing worth uploading.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarPayload {
    pub part_id: String,
    pub output: Option<String>,
    pub diff: Option<ToolDiff>,
}

pub fn sidecar_payload(event: &AgentEvent) -> Option<SidecarPayload> {
    let AgentEvent::ToolResult {
        id, output, diff, ..
    } = event
    else {
        return None;
    };
    let output = output.clone().filter(|o| !o.trim().is_empty());
    if output.is_none() && diff.is_none() {
        return None;
    }
    Some(SidecarPayload {
        part_id: id.clone(),
        output,
        diff: diff.clone(),
    })
}

/// Render-only privacy policy — strip heavy/sensitive tool inputs before a call enters the doc.
///
/// Keeps: command / path / pattern / url / query / todo items / server+tool names / Mcp subagent model keys.
/// Drops: WriteFile content, EditFile old/new strings, WebFetch prompt, other Mcp/Unknown input.
/// Full inputs remain only in the host's local run journal. Idempotent.
pub fn sanitize_tool_call(call: &ToolCall) -> ToolCall {
    match call {
        ToolCall::WriteFile { path, .. } => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        ToolCall::EditFile { path, .. } => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        ToolCall::WebFetch { url, .. } => ToolCall::WebFetch {
            url: url.clone(),
            prompt: None,
        },
        ToolCall::Mcp {
            server,
            tool,
            input,
        } => ToolCall::Mcp {
            server: server.clone(),
            tool: tool.clone(),
            input: input.as_ref().and_then(|v| {
                let kept: serde_json::Map<String, serde_json::Value> = SUBAGENT_MODEL_KEYS
                    .iter()
                    .filter_map(|k| v.get(*k).map(|val| (k.to_string(), val.clone())))
                    .collect();
                if kept.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(kept))
                }
            }),
        },
        ToolCall::Unknown { name, .. } => ToolCall::Unknown {
            name: name.clone(),
            input: None,
        },
        other => other.clone(),
    }
}

/// Deterministic continuation id: `"{root}#c{n}"`.
pub fn continuation_id(root: &str, index: usize) -> String {
    format!("{root}#c{index}")
}

/// Split an oversized parts list into chunks each under `MSG_INLINE_MAX` bytes.
///
/// Splitting happens at part boundaries; an oversized text part is itself chunked at char
/// boundaries. Returns one Vec per resulting entry — the first keeps the root id, the rest are
/// continuations (`continuation_id(root, i)`), matching `splitMessageEntry` in komet.
pub fn split_parts(parts: &[MessagePart]) -> Vec<Vec<MessagePart>> {
    let mut chunks: Vec<Vec<MessagePart>> = vec![Vec::new()];
    let mut current_bytes = 0usize;

    let push_part = |chunks: &mut Vec<Vec<MessagePart>>, current: &mut usize, part: MessagePart| {
        let len = part.byte_len();
        if *current > 0 && *current + len > MSG_INLINE_MAX {
            chunks.push(Vec::new());
            *current = 0;
        }
        *current += len;
        chunks.last_mut().unwrap().push(part);
    };

    for part in parts {
        match part {
            MessagePart::Text { id, text } if text.len() > MSG_INLINE_MAX => {
                // Chunk oversized text at char boundaries.
                let mut start = 0usize;
                let mut piece = 0usize;
                while start < text.len() {
                    let mut end = (start + MSG_INLINE_MAX).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    // Guard: ensure forward progress on pathological boundaries.
                    if end <= start {
                        end = text.len();
                    }
                    let sub = MessagePart::Text {
                        id: if piece == 0 {
                            id.clone()
                        } else {
                            format!("{id}~{piece}")
                        },
                        text: text[start..end].to_string(),
                    };
                    push_part(&mut chunks, &mut current_bytes, sub);
                    start = end;
                    piece += 1;
                }
            }
            other => push_part(&mut chunks, &mut current_bytes, other.clone()),
        }
    }
    chunks
}

/// Render-time inverse of splitting: concatenate continuation entries' parts in list order.
pub fn join_continuations(entries: Vec<Vec<MessagePart>>) -> Vec<MessagePart> {
    entries.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_errored_after_identical_error_keeps_single_detailed_part() {
        let mut out = Vec::new();
        fold_event_into_parts(
            &mut out,
            &AgentEvent::Error {
                message: "boom".into(),
                kind: ErrorKind::Timeout,
                detail: Some("tail".into()),
                retryable: true,
            },
        );
        fold_event_into_parts(
            &mut out,
            &AgentEvent::Done {
                status: komet_proto::DoneStatus::Errored,
                result: None,
                error: Some("boom".into()),
                session_id: None,
                reason: None,
            },
        );
        assert_eq!(
            out.len(),
            1,
            "identical Done error must not duplicate the part: {out:?}"
        );
        assert!(
            matches!(&out[0], MessagePart::Error { detail: Some(d), .. } if d == "tail"),
            "the surviving part keeps the stderr detail: {:?}",
            out[0]
        );
    }

    fn text_delta(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    #[test]
    fn text_deltas_merge_until_broken_by_tool() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("Hello "));
        fold_event_into_parts(&mut parts, &text_delta("world"));
        assert_eq!(parts.len(), 1);
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(&mut parts, &text_delta("after"));
        assert_eq!(parts.len(), 3);
        match &parts[2] {
            MessagePart::Text { text, .. } => assert_eq!(text, "after"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn session_started_resets_accumulator() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("junk"));
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::SessionStarted {
                harness: komet_proto::HarnessId::Mock,
                model: "m".into(),
                tools: vec![],
                cwd: "/".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
            },
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn reasoning_delta_produces_zero_parts() {
        // Issue #31 guard: reasoning streams live-only; the fold must keep
        // dropping it so nothing new lands in the synced doc.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ReasoningDelta {
                text: "thinking out loud".into(),
            },
        );
        assert!(parts.is_empty());
        fold_event_into_parts(&mut parts, &text_delta("visible"));
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn tool_call_refresh_is_idempotent() {
        let call = AgentEvent::ToolCall {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
        };
        let mut once = Vec::new();
        fold_event_into_parts(&mut once, &call);
        let mut twice = once.clone();
        fold_event_into_parts(&mut twice, &call);
        assert_eq!(once, twice);
    }

    #[test]
    fn tool_call_refresh_never_downgrades_to_unknown() {
        // Title-only completion updates (no kind/rawInput) must not clobber
        // a typed chip — e.g. a Todo into an "N todos" JSON dump.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Todo { items: vec![] },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Unknown {
                    name: "3 todos".into(),
                    input: None,
                },
            },
        );
        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                call: ToolCall::Todo { .. },
                ..
            }
        ));
        // Unknown-to-Unknown still refreshes (titles carry live progress).
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "u".into(),
                call: ToolCall::Unknown {
                    name: "old".into(),
                    input: None,
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "u".into(),
                call: ToolCall::Unknown {
                    name: "new".into(),
                    input: None,
                },
            },
        );
        assert!(matches!(
            &parts[1],
            MessagePart::Tool {
                call: ToolCall::Unknown { name, .. },
                ..
            } if name == "new"
        ));
    }

    #[test]
    fn tool_result_marks_resolution() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
                output: None,
                diff: None,
            },
        );
        match &parts[0] {
            MessagePart::Tool {
                is_error, resolved, ..
            } => {
                assert!(*is_error);
                assert!(*resolved);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sanitize_strips_heavy_inputs_and_is_idempotent() {
        let call = ToolCall::WriteFile {
            path: "/x".into(),
            content: Some("secret".into()),
        };
        let clean = sanitize_tool_call(&call);
        assert_eq!(
            clean,
            ToolCall::WriteFile {
                path: "/x".into(),
                content: None
            }
        );
        assert_eq!(sanitize_tool_call(&clean), clean);
    }

    #[test]
    fn sanitize_mcp_keeps_only_subagent_model_keys() {
        let call = ToolCall::Mcp {
            server: "s".into(),
            tool: "t".into(),
            input: Some(serde_json::json!({"model": "sonnet", "prompt": "secret"})),
        };
        assert_eq!(
            sanitize_tool_call(&call),
            ToolCall::Mcp {
                server: "s".into(),
                tool: "t".into(),
                input: Some(serde_json::json!({"model": "sonnet"})),
            }
        );
        let no_model = ToolCall::Mcp {
            server: "s".into(),
            tool: "t".into(),
            input: Some(serde_json::json!({"prompt": "secret"})),
        };
        assert_eq!(
            sanitize_tool_call(&no_model),
            ToolCall::Mcp {
                server: "s".into(),
                tool: "t".into(),
                input: None,
            }
        );
    }

    #[test]
    fn split_and_join_round_trip() {
        let big = "x".repeat(MSG_INLINE_MAX * 2 + 100);
        let parts = vec![
            MessagePart::Text {
                id: "t0".into(),
                text: big.clone(),
            },
            MessagePart::Tool {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                subagent_duration_ms: None,
                subagent_tool_count: None,
            },
        ];
        let chunks = split_parts(&parts);
        assert!(
            chunks.len() >= 3,
            "expected >=3 chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|p| p.byte_len()).sum();
            assert!(bytes <= MSG_INLINE_MAX, "chunk over cap: {bytes}");
        }
        let joined = join_continuations(chunks);
        let text: String = joined
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, big);
        assert!(matches!(joined.last().unwrap(), MessagePart::Tool { .. }));
    }

    #[test]
    fn continuation_ids_are_deterministic() {
        assert_eq!(continuation_id("m1", 1), "m1#c1");
    }

    // ── A1 strip (docs/chat2-sync.md) ───────────────────────────────────────

    #[test]
    fn summarize_inlines_small_outputs_and_marks_big_cuts() {
        assert_eq!(summarize_tool_output(""), None);
        assert_eq!(summarize_tool_output("  \n\t\n"), None);
        assert_eq!(summarize_tool_output("one line"), Some("one line".into()));
        // Small multi-line outputs ride whole — no summary, no "…".
        assert_eq!(
            summarize_tool_output("\n\nfirst real\nsecond"),
            Some("first real\nsecond".into())
        );
        assert_eq!(
            summarize_tool_output("only line\n\n  \n"),
            Some("only line".into())
        );
        // Markdown fences are transport wrapping, never content: stripped
        // even when they'd otherwise be the first line, and a fence-only
        // output is blank.
        assert_eq!(
            summarize_tool_output("```console\nreal content\n```"),
            Some("real content".into())
        );
        assert_eq!(summarize_tool_output("```\n```"), None);
        // Big outputs: first non-empty (post-fence) line + unconditional "…".
        let big = format!("```console\nhead line\n{}\n```", "x".repeat(300));
        assert_eq!(summarize_tool_output(&big), Some("head line…".into()));
        let long = "x".repeat(TOOL_OUTPUT_SUMMARY_MAX + 40);
        let summary = summarize_tool_output(&long).unwrap();
        assert_eq!(summary.chars().count(), TOOL_OUTPUT_SUMMARY_MAX + 1);
        assert!(summary.ends_with('…'));
        // Char-boundary safety on multibyte input.
        let wide = "é".repeat(TOOL_OUTPUT_SUMMARY_MAX + 5);
        let summary = summarize_tool_output(&wide).unwrap();
        assert_eq!(summary.chars().count(), TOOL_OUTPUT_SUMMARY_MAX + 1);
    }

    #[test]
    fn diff_stat_counts_line_changes() {
        let stat = diff_stat(&ToolDiff {
            path: "/w/a.rs".into(),
            old_text: Some("a\nb\nc\n".into()),
            new_text: "a\nB\nc\nd\n".into(),
        });
        assert_eq!(stat.path, "/w/a.rs");
        assert_eq!(stat.additions, 2); // B + d
        assert_eq!(stat.deletions, 1); // b
        // New file: every line is an addition.
        let stat = diff_stat(&ToolDiff {
            path: "/w/new.rs".into(),
            old_text: None,
            new_text: "one\ntwo\n".into(),
        });
        assert_eq!((stat.additions, stat.deletions), (2, 0));
    }

    #[test]
    fn error_tool_result_keeps_160_summary_only() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        let long = "y".repeat(200);
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
                output: Some(long),
                diff: None,
            },
        );
        let summary = parts.iter().find_map(|p| match p {
            MessagePart::Tool { output, .. } => output.clone(),
            _ => None,
        });
        assert_eq!(summary, Some(format!("{}…", "y".repeat(160))));

        // ... while a successful result with output persists no summary.
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "u".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "u".into(),
                is_error: false,
                output: Some("y".repeat(200)),
                diff: None,
            },
        );
        let summary = parts.iter().find_map(|p| match p {
            MessagePart::Tool { output, .. } => output.clone(),
            _ => None,
        });
        assert_eq!(summary, None);
    }

    #[test]
    fn fold_strips_output_to_summary_and_diff_to_stats() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "cargo test".into(),
                },
            },
        );
        let full = "running 42 tests\n".repeat(300); // ~5KB, was 4KB inline pre-strip
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: false,
                output: Some(full.clone()),
                diff: Some(ToolDiff {
                    path: "/w/a.rs".into(),
                    old_text: Some("a\n".into()),
                    new_text: "b\n".into(),
                }),
            },
        );
        match &parts[0] {
            MessagePart::Tool {
                output,
                output_bytes,
                diff,
                diff_stats,
                ..
            } => {
                // One-liner chips: outputs never enter the doc at all
                // (journal-only); diff text neither — stats survive.
                assert_eq!(output.as_deref(), None);
                assert_eq!(*output_bytes, None);
                assert!(diff.is_none(), "inline diff text must not enter the doc");
                let stats = diff_stats.as_ref().unwrap();
                assert_eq!(stats.len(), 1);
                assert_eq!((stats[0].additions, stats[0].deletions), (1, 1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sidecar_refs_stamp_once_and_only_where_content_exists() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        // Unresolved: no refs yet.
        apply_sidecar_refs("chat-9", &mut parts);
        assert!(matches!(
            &parts[0],
            MessagePart::Tool {
                output_ref: None,
                diff_ref: None,
                ..
            }
        ));
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: false,
                output: Some("hello".into()),
                diff: Some(ToolDiff {
                    path: "/w/a".into(),
                    old_text: None,
                    new_text: "x\n".into(),
                }),
            },
        );
        apply_sidecar_refs("chat-9", &mut parts);
        apply_sidecar_refs("chat-9", &mut parts); // idempotent
        match &parts[0] {
            MessagePart::Tool {
                output_ref,
                diff_ref,
                ..
            } => {
                // One-liner fold: outputs never reach the doc, so there is
                // no output content to key even after resolution; diff
                // STATS exist, so the diff ref still stamps.
                assert_eq!(output_ref.as_deref(), None);
                assert_eq!(diff_ref.as_deref(), Some("chat-9/t1.diff"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sidecar_payload_carries_full_texts() {
        assert_eq!(
            sidecar_payload(&AgentEvent::TextDelta { text: "x".into() }),
            None
        );
        assert_eq!(
            sidecar_payload(&AgentEvent::ToolResult {
                id: "t".into(),
                is_error: false,
                output: Some("   \n".into()),
                diff: None,
            }),
            None,
            "blank output uploads nothing"
        );
        let payload = sidecar_payload(&AgentEvent::ToolResult {
            id: "t".into(),
            is_error: true,
            output: Some("full output".into()),
            diff: None,
        })
        .unwrap();
        assert_eq!(payload.part_id, "t");
        assert_eq!(payload.output.as_deref(), Some("full output"));
    }

    fn todo_entry(
        id: &str,
        role: crate::schema::MessageRole,
        items: Option<Vec<komet_proto::TodoItem>>,
    ) -> crate::schema::SessionMessageEntry {
        let parts = match items {
            Some(items) => vec![MessagePart::Tool {
                id: "acp-plan".into(),
                call: ToolCall::Todo { items },
                is_error: false,
                resolved: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                subagent_duration_ms: None,
                subagent_tool_count: None,
            }],
            None => vec![MessagePart::Text {
                id: "t0".into(),
                text: "hi".into(),
            }],
        };
        crate::schema::SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn todo(text: &str, done: bool) -> komet_proto::TodoItem {
        komet_proto::TodoItem {
            text: text.into(),
            done,
        }
    }

    #[test]
    fn latest_plan_is_none_without_a_todo_call() {
        use crate::schema::MessageRole;
        let entries = vec![todo_entry("m1", MessageRole::User, None)];
        assert_eq!(latest_plan(&entries), None);
    }

    #[test]
    fn latest_plan_returns_the_last_todo_snapshot() {
        use crate::schema::MessageRole;
        let first = vec![todo("a", false), todo("b", false)];
        let last = vec![todo("a", true), todo("b", true)];
        let entries = vec![
            todo_entry("m1", MessageRole::User, None),
            todo_entry("m2", MessageRole::Assistant, Some(first)),
            todo_entry("m3", MessageRole::Assistant, Some(last.clone())),
        ];
        assert_eq!(latest_plan(&entries), Some(last));
    }

    #[test]
    fn latest_plan_keeps_a_plan_across_later_text_entries() {
        use crate::schema::MessageRole;
        let plan = vec![todo("a", false)];
        let entries = vec![
            todo_entry("m1", MessageRole::Assistant, Some(plan.clone())),
            todo_entry("m2", MessageRole::User, None),
        ];
        assert_eq!(latest_plan(&entries), Some(plan));
    }

    #[test]
    fn latest_plan_empty_list_means_cleared() {
        use crate::schema::MessageRole;
        let entries = vec![
            todo_entry("m1", MessageRole::Assistant, Some(vec![todo("a", false)])),
            todo_entry("m2", MessageRole::Assistant, Some(vec![])),
        ];
        assert_eq!(latest_plan(&entries), Some(vec![]));
    }

    /// A compaction frame with every optional field defaulting to "the agent
    /// did not say" — the fold's whole difficulty is what it may not invent.
    #[derive(Default)]
    struct Compaction {
        id: &'static str,
        status: CompactionStatus,
        trigger: Option<&'static str>,
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
        summary: Option<&'static str>,
        summary_delta: Option<&'static str>,
        error: Option<&'static str>,
    }

    fn compaction_frame(f: Compaction) -> AgentEvent {
        AgentEvent::ContextCompaction {
            id: f.id.into(),
            status: f.status,
            trigger: f.trigger.map(str::to_owned),
            tokens_before: f.tokens_before,
            tokens_after: f.tokens_after,
            summary: f.summary.map(str::to_owned),
            summary_delta: f.summary_delta.map(str::to_owned),
            error: f.error.map(str::to_owned),
        }
    }

    fn fold_compaction_frame(out: &mut Vec<MessagePart>, f: Compaction) {
        fold_event_into_parts(out, &compaction_frame(f));
    }

    fn part(out: &[MessagePart]) -> &MessagePart {
        &out[0]
    }

    #[test]
    fn compaction_fold_updates_in_place_by_id() {
        let mut out = Vec::new();
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                status: CompactionStatus::InProgress,
                trigger: Some("auto"),
                tokens_before: Some(165_000),
                ..Default::default()
            },
        );
        assert_eq!(out.len(), 1, "an in-progress compaction is one part");
        let MessagePart::Compaction {
            id,
            compaction_id,
            status,
            trigger,
            tokens_before,
            tokens_after,
            summary,
            summary_bytes,
            summary_in_journal,
            ..
        } = part(&out)
        else {
            panic!("expected a compaction part, got {:?}", out[0]);
        };
        assert_eq!((id.as_str(), compaction_id.as_str()), ("cmp-c1", "c1"));
        assert_eq!(*status, CompactionStatus::InProgress);
        assert_eq!(trigger.as_deref(), Some("auto"));
        assert_eq!(tokens_before, &Some(165_000));
        // Unknown is unknown: no arrow, no invented post-compaction count.
        assert_eq!(tokens_after, &None);
        assert_eq!(summary, &None);
        assert_eq!(summary_bytes, &None);
        assert!(!summary_in_journal, "nothing was summarized yet");

        // Completion lands on the SAME part and must not erase what the first
        // frame carried.
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                status: CompactionStatus::Completed,
                tokens_after: Some(12_000),
                summary: Some("they met"),
                ..Default::default()
            },
        );
        assert_eq!(out.len(), 1, "the cycle refines its own part: {out:?}");
        let MessagePart::Compaction {
            status,
            trigger,
            tokens_before,
            tokens_after,
            summary_bytes,
            summary_in_journal,
            ..
        } = part(&out)
        else {
            unreachable!()
        };
        assert_eq!(*status, CompactionStatus::Completed);
        assert_eq!(trigger.as_deref(), Some("auto"));
        assert_eq!(tokens_before, &Some(165_000));
        assert_eq!(tokens_after, &Some(12_000));
        assert_eq!(summary_bytes, &Some(8));
        assert!(*summary_in_journal);

        // A streamed chunk extends the accumulated size (text stays out of the
        // doc under the default policy).
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                summary_delta: Some(" more"),
                ..Default::default()
            },
        );
        let MessagePart::Compaction {
            summary,
            summary_bytes,
            ..
        } = part(&out)
        else {
            unreachable!()
        };
        assert_eq!(summary, &None, "JournalOnly never writes text");
        assert_eq!(summary_bytes, &Some(13));

        // A settled cycle is not reopened by a late in-progress frame…
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                status: CompactionStatus::InProgress,
                ..Default::default()
            },
        );
        let MessagePart::Compaction { status, .. } = part(&out) else {
            unreachable!()
        };
        assert_eq!(*status, CompactionStatus::Completed);

        // …but the agent correcting itself with a failure is believed.
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                status: CompactionStatus::Failed,
                error: Some("agent died"),
                ..Default::default()
            },
        );
        let MessagePart::Compaction { status, error, .. } = part(&out) else {
            unreachable!()
        };
        assert_eq!(*status, CompactionStatus::Failed);
        assert_eq!(error.as_deref(), Some("agent died"));
    }

    #[test]
    fn two_compactions_in_one_run_get_two_parts() {
        let mut out = Vec::new();
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                status: CompactionStatus::Completed,
                tokens_before: Some(165_000),
                ..Default::default()
            },
        );
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c2",
                status: CompactionStatus::InProgress,
                ..Default::default()
            },
        );
        assert_eq!(out.len(), 2, "one session, two boundaries");
        assert_eq!((out[0].id(), out[1].id()), ("cmp-c1", "cmp-c2"));
        let MessagePart::Compaction {
            status: first,
            tokens_before: first_before,
            ..
        } = &out[0]
        else {
            unreachable!()
        };
        let MessagePart::Compaction {
            status: second,
            tokens_before: second_before,
            ..
        } = &out[1]
        else {
            unreachable!()
        };
        // The second cycle must not clobber the first one's facts.
        assert_eq!(*first, CompactionStatus::Completed);
        assert_eq!(*first_before, Some(165_000));
        assert_eq!(*second, CompactionStatus::InProgress);
        assert_eq!(*second_before, None);
    }

    #[test]
    fn journal_only_summary_never_reaches_the_doc() {
        let mut out = Vec::new();
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                status: CompactionStatus::Completed,
                summary: Some("a very large model-written recap"),
                ..Default::default()
            },
        );
        let MessagePart::Compaction {
            summary,
            summary_bytes,
            summary_in_journal,
            ..
        } = part(&out)
        else {
            unreachable!()
        };
        assert_eq!(summary, &None, "the default policy keeps the text out");
        assert_eq!(summary_bytes, &Some(32), "but the size is known");
        assert!(
            *summary_in_journal,
            "a summary exists, so there is something to fetch"
        );
    }

    #[test]
    fn summary_in_doc_is_capped_on_a_utf8_boundary() {
        // U+20AC is 3 bytes: 8192 is not a char boundary here, so a naive byte
        // cut would tear the last euro in half.
        let huge: String = "€".repeat(100 * 1024 / 3 + 1);
        let mut out = Vec::new();
        fold_event_into_parts_with(
            &mut out,
            &AgentEvent::ContextCompaction {
                id: "c1".into(),
                status: CompactionStatus::Completed,
                trigger: None,
                tokens_before: None,
                tokens_after: None,
                summary: Some(huge.clone()),
                summary_delta: None,
                error: None,
            },
            CompactionSummaryPolicy::SummaryInDoc,
        );
        let MessagePart::Compaction {
            summary,
            summary_bytes,
            ..
        } = part(&out)
        else {
            unreachable!()
        };
        let kept = summary.as_ref().expect("opt-in writes the text");
        assert_eq!(
            kept.len(),
            COMPACTION_SUMMARY_MAX_BYTES - 2,
            "8190, not 8192: the cut backed off to the last whole character"
        );
        assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
        assert_eq!(
            summary_bytes,
            &Some(huge.len() as u64),
            "the cap is visible: the doc says how much it dropped"
        );

        // Deltas accumulate into the same capped field, in order.
        let mut streamed = Vec::new();
        for chunk in ["first ", "second ", "third"] {
            fold_event_into_parts_with(
                &mut streamed,
                &compaction_frame(Compaction {
                    id: "c2",
                    summary_delta: Some(chunk),
                    ..Default::default()
                }),
                CompactionSummaryPolicy::SummaryInDoc,
            );
        }
        let MessagePart::Compaction { summary, .. } = part(&streamed) else {
            unreachable!()
        };
        assert_eq!(summary.as_deref(), Some("first second third"));
    }

    #[test]
    fn compaction_part_does_not_disturb_other_parts() {
        let mut out = Vec::new();
        fold_event_into_parts(&mut out, &AgentEvent::TextDelta { text: "hi".into() });
        fold_compaction_frame(
            &mut out,
            Compaction {
                id: "c1",
                ..Default::default()
            },
        );
        fold_event_into_parts(
            &mut out,
            &AgentEvent::TextDelta {
                text: " there".into(),
            },
        );
        // A compaction is a real boundary in the segment: the text after it is
        // its own part, never appended under the row.
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(matches!(&out[1], MessagePart::Compaction { .. }));
    }

    #[test]
    fn permission_fold_carries_the_watchdog_deadline() {
        let mut out = Vec::new();
        fold_event_into_parts(
            &mut out,
            &AgentEvent::PermissionRequested {
                request_id: "r1".into(),
                kind: komet_proto::PermissionKind::Command {
                    cmdline: "rm -rf dist".into(),
                },
                summary: "Run `rm -rf dist`".into(),
                choices: vec![
                    komet_proto::PermissionChoice::Deny,
                    komet_proto::PermissionChoice::Allow,
                ],
                actions: vec![],
                provider: None,
                tool_call_id: None,
                expires_at: Some(1_700_000_600_000),
            },
        );
        let MessagePart::Permission {
            expires_at,
            resolved,
            ..
        } = &out[0]
        else {
            panic!("permission part expected");
        };
        assert_eq!(*expires_at, Some(1_700_000_600_000));
        assert!(!*resolved, "the countdown rides the open request");
        // The watchdog's own Deny resolves the same part.
        fold_event_into_parts(
            &mut out,
            &AgentEvent::PermissionResolved {
                request_id: "r1".into(),
                choice: komet_proto::PermissionChoice::Deny,
            },
        );
        let MessagePart::Permission {
            resolved, decision, ..
        } = &out[0]
        else {
            panic!();
        };
        assert!(*resolved);
        assert!(matches!(
            decision,
            Some(komet_proto::PermissionChoice::Deny)
        ));
    }
}
