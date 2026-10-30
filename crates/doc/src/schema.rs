//! Session doc schema over `loro` — Rust port of `packages/session-doc/src/schema.ts`.
//!
//! Container layout (MUST stay shape-compatible with the TS edge/tail materializer):
//! - `meta`:     LoroMap  { chatId: string, schemaVersion: number }         (host-only writer)
//! - `messages`: LoroList of LoroMap {
//!   id, role, parts: LoroList<part map>, createdAt, deviceId, status?, continuationOf? }
//! - `commands`: LoroList of LoroMap {
//!   id, kind, payload(json), issuedBy, issuedAt, basedOn?, expiresAt?, status, resolution? }
//!
//! Part maps: { id, kind: "text"|"tool"|"input"|"error", text?: LoroText, call?: json,
//! isError?, questions?: json, resolved?, message? }. Text bodies are **LoroText** so streaming
//! appends RLE-merge (1.03x oplog overhead vs 125x for whole-value rewrites).

use std::collections::BTreeMap;

use loro::{ExportMode, LoroDoc, LoroError, LoroList, LoroMap, LoroText, LoroValue, ToJson};
use serde::{Deserialize, Serialize};

use crate::commands::{SessionCommandEntry, SessionCommandStatus};
use crate::constants::{SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT};
use crate::parts::{InputResponse, MessagePart, MessageStatus};

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("loro: {0}")]
    Loro(#[from] LoroError),
    #[error("schema: {0}")]
    Schema(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionMark {
    pub session_id: String,
    pub cwd: String,
    pub seen: Option<String>,
}

/// One entry in the doc's `messages` list (`SessionMessageEntry` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    pub id: String,
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    /// Epoch millis.
    pub created_at: i64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_of: Option<String>,
}

/// The doc-resident flat part map (`DocMessagePart` in TS). Distinct from the app-layer
/// [`MessagePart`]: input parts key on their request id, error parts store `message`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocPartJson {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    questions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Tool output summary (additive — absent on old rows and old writers;
    /// pre-strip writers stored up to 4KB of capped output here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    /// Capped inline tool diff (additive; pre-strip writers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diff: Option<serde_json::Value>,
    /// Sidecar key of the full output (additive, docs/chat2-sync.md A1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_ref: Option<String>,
    /// Full-output byte length (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
    /// Sidecar key of the full diff JSON (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diff_ref: Option<String>,
    /// Per-file diff stats (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diff_stats: Option<serde_json::Value>,
    /// Subagent lifecycle fields (schema parity with `MessagePart::Tool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_status: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_tail: Option<String>,
    /// Terminal cycle numbers of a spawned subagent, stamped by the host in the
    /// chip's single terminal write (additive; absent on every older row, and
    /// on a chip whose run died before its terminal event — `None` is "not
    /// measured here", never "took zero").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_tool_count: Option<u64>,
    /// Dedicated request_id for permission parts (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    /// Typed error class for error parts (additive; absent on old rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_kind: Option<komet_proto::ErrorKind>,
    /// Raw CLI text for error parts (additive; absent on old rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_detail: Option<String>,
    /// Retry flag for error parts (additive; absent on old rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_retryable: Option<bool>,
    /// Answers recorded against an input part, as a doc-resident list so two
    /// devices answering the same question both keep their answer (additive;
    /// absent on old rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    responses: Option<serde_json::Value>,
    /// Which wire a question came in on (additive; absent means native).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<komet_proto::QuestionSource>,
    /// The user's decision on a permission part (additive; parts written
    /// before this column exist resolved-but-undecided, which the transcript
    /// renders as "no decision recorded" rather than inventing one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<komet_proto::PermissionChoice>,
    /// The tool call a permission gates (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Device that decided, and its clock at the time (additive, display only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decided_at: Option<i64>,
    /// Compaction boundary fields (additive; absent on every older row). A doc
    /// written before this kind exists simply has no such part; a reader that
    /// predates the kind falls through to an empty text part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_status: Option<komet_proto::CompactionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary_in_journal: Option<bool>,
}

/// App parts → doc part json (mirror of `toDocParts`).
fn to_doc_part(part: &MessagePart) -> Result<DocPartJson, DocError> {
    Ok(match part {
        MessagePart::Text { id, text } => DocPartJson {
            id: id.clone(),
            kind: "text".into(),
            text: Some(text.clone()),
            ..Default::default()
        },
        MessagePart::Tool {
            id,
            call,
            is_error,
            resolved,
            output,
            diff,
            output_ref,
            output_bytes,
            diff_ref,
            diff_stats,
            subagent_ref,
            subagent_status,
            subagent_tail,
            subagent_duration_ms,
            subagent_tool_count,
        } => DocPartJson {
            id: id.clone(),
            kind: "tool".into(),
            call: Some(serde_json::to_value(call)?),
            // TS shape parity: `isError` is written only once the tool result arrived;
            // its presence IS the resolution marker.
            is_error: if *resolved { Some(*is_error) } else { None },
            output: output.clone(),
            diff: diff.as_ref().map(serde_json::to_value).transpose()?,
            output_ref: output_ref.clone(),
            output_bytes: *output_bytes,
            diff_ref: diff_ref.clone(),
            diff_stats: diff_stats.as_ref().map(serde_json::to_value).transpose()?,
            subagent_ref: subagent_ref.clone(),
            subagent_status: subagent_status
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?,
            subagent_tail: subagent_tail.clone(),
            subagent_duration_ms: *subagent_duration_ms,
            subagent_tool_count: *subagent_tool_count,
            ..Default::default()
        },
        MessagePart::Input {
            id: _,
            request_id,
            questions,
            resolved,
            responses,
            source,
        } => DocPartJson {
            id: request_id.clone(),
            kind: "input".into(),
            questions: Some(serde_json::to_value(questions)?),
            resolved: Some(*resolved),
            responses: (!responses.is_empty())
                .then(|| serde_json::to_value(responses))
                .transpose()?,
            source: (!source.is_native()).then_some(*source),
            ..Default::default()
        },
        MessagePart::Switch {
            id,
            from,
            to,
            from_harness,
            to_harness,
        } => DocPartJson {
            id: id.clone(),
            kind: "switch".into(),
            message: Some(format!("{from} → {to}")),
            // store the switch metadata as the request tuple for round-tripping
            questions: Some(serde_json::to_value((from, to, from_harness, to_harness))?),
            ..Default::default()
        },
        MessagePart::Error {
            id,
            message,
            kind,
            detail,
            retryable,
        } => DocPartJson {
            id: id.clone(),
            kind: "error".into(),
            message: Some(message.clone()),
            error_kind: (!kind.is_unknown()).then_some(*kind),
            error_detail: detail.clone(),
            error_retryable: (*retryable).then_some(true),
            ..Default::default()
        },
        MessagePart::Permission {
            id,
            request_id,
            permission_kind,
            summary,
            choices,
            actions,
            resolved,
            decision,
            tool_call_id,
            decided_by,
            decided_at,
            // Host-local watchdog deadline: ephemeral, never written to the
            // store (see the projection's `expires_at: None`).
            expires_at: _,
        } => DocPartJson {
            id: id.clone(),
            kind: "permission".into(),
            message: Some(summary.clone()),
            questions: Some(serde_json::to_value((
                permission_kind,
                choices,
                request_id,
                actions,
            ))?),
            resolved: Some(*resolved),
            request_id: Some(request_id.clone()),
            decision: decision.clone(),
            tool_call_id: tool_call_id.clone(),
            decided_by: decided_by.clone(),
            decided_at: *decided_at,
            ..Default::default()
        },
        MessagePart::Compaction {
            id,
            compaction_id,
            status,
            trigger,
            tokens_before,
            tokens_after,
            error,
            summary,
            summary_bytes,
            summary_in_journal,
        } => DocPartJson {
            id: id.clone(),
            kind: "compaction".into(),
            compaction_id: Some(compaction_id.clone()),
            compaction_status: Some(*status),
            trigger: trigger.clone(),
            tokens_before: *tokens_before,
            tokens_after: *tokens_after,
            compaction_error: error.clone(),
            summary: summary.clone(),
            summary_bytes: *summary_bytes,
            summary_in_journal: summary_in_journal.then_some(true),
            ..Default::default()
        },
    })
}

/// Doc part json → app part (mirror of `fromDocParts`; malformed degrades to empty text).
fn from_doc_part(p: DocPartJson) -> MessagePart {
    match p.kind.as_str() {
        "tool" => match p.call.and_then(|c| serde_json::from_value(c).ok()) {
            Some(call) => MessagePart::Tool {
                id: p.id,
                call,
                is_error: p.is_error.unwrap_or(false),
                resolved: p.is_error.is_some(),
                output: p.output,
                diff: p.diff.and_then(|d| serde_json::from_value(d).ok()),
                output_ref: p.output_ref,
                output_bytes: p.output_bytes,
                diff_ref: p.diff_ref,
                diff_stats: p.diff_stats.and_then(|s| serde_json::from_value(s).ok()),
                subagent_ref: p.subagent_ref,
                subagent_status: p
                    .subagent_status
                    .and_then(|s| serde_json::from_value(s).ok()),
                subagent_tail: p.subagent_tail,
                subagent_duration_ms: p.subagent_duration_ms,
                subagent_tool_count: p.subagent_tool_count,
            },
            None => MessagePart::Text {
                id: p.id,
                text: String::new(),
            },
        },
        "input" => MessagePart::Input {
            id: p.id.clone(),
            request_id: p.id,
            questions: p
                .questions
                .and_then(|q| serde_json::from_value(q).ok())
                .unwrap_or_default(),
            resolved: p.resolved.unwrap_or(false),
            responses: p
                .responses
                .and_then(|r| serde_json::from_value(r).ok())
                .unwrap_or_default(),
            source: p.source.unwrap_or_default(),
        },
        "switch" => {
            let tuple: Option<(String, String, Option<String>, Option<String>)> =
                p.questions.and_then(|q| serde_json::from_value(q).ok());
            match tuple {
                Some((from, to, from_harness, to_harness)) => MessagePart::Switch {
                    id: p.id,
                    from,
                    to,
                    from_harness,
                    to_harness,
                },
                None => MessagePart::Text {
                    id: p.id,
                    text: p.message.unwrap_or_default(),
                },
            }
        }
        "error" => MessagePart::Error {
            id: p.id,
            message: p.message.unwrap_or_default(),
            kind: p.error_kind.unwrap_or_default(),
            detail: p.error_detail,
            retryable: p.error_retryable.unwrap_or(false),
        },
        "permission" => {
            // Read request_id directly from the dedicated column first
            let request_id = if let Some(rid) = p.request_id {
                rid
            } else {
                // Legacy fallback: extract from the tuple's third element
                let tuple_rid: Option<String> = p
                    .questions
                    .as_ref()
                    .and_then(|q| {
                        serde_json::from_value::<(
                            serde_json::Value,
                            serde_json::Value,
                            String,
                            serde_json::Value,
                        )>(q.clone())
                        .ok()
                    })
                    .map(|(_, _, rid, _)| {
                        // Strip the perm- prefix in legacy fallback
                        rid.strip_prefix("perm-").unwrap_or(&rid).to_string()
                    });
                tuple_rid.unwrap_or_else(|| p.id.clone())
            };
            let (kind, choices, _tuple_request_id, actions): (
                komet_proto::PermissionKind,
                Vec<komet_proto::PermissionChoice>,
                String,
                Vec<komet_proto::AgentPermissionAction>,
            ) = p
                .questions
                .and_then(|q| serde_json::from_value(q).ok())
                .unwrap_or((
                    komet_proto::PermissionKind::Command {
                        cmdline: String::new(),
                    },
                    vec![],
                    request_id.clone(),
                    vec![],
                ));
            MessagePart::Permission {
                id: p.id,
                request_id,
                permission_kind: kind,
                summary: p.message.unwrap_or_default(),
                choices,
                actions,
                resolved: p.resolved.unwrap_or(false),
                decision: p.decision,
                tool_call_id: p.tool_call_id,
                decided_by: p.decided_by,
                decided_at: p.decided_at,
                // Host-local watchdog deadline: never persisted. A restart
                // force-resolves the request with the run, so there is no
                // countdown to restore.
                expires_at: None,
            }
        }
        "compaction" => {
            // A row that only carries the doc key still names its cycle: the
            // id is `cmp-{compactionId}` by construction.
            let compaction_id = p.compaction_id.clone().unwrap_or_else(|| {
                p.id.strip_prefix("cmp-")
                    .unwrap_or(p.id.as_str())
                    .to_owned()
            });
            MessagePart::Compaction {
                id: p.id,
                compaction_id,
                status: p.compaction_status.unwrap_or_default(),
                trigger: p.trigger,
                tokens_before: p.tokens_before,
                tokens_after: p.tokens_after,
                error: p.compaction_error,
                summary: p.summary,
                summary_bytes: p.summary_bytes,
                summary_in_journal: p.summary_in_journal.unwrap_or(false),
            }
        }
        _ => MessagePart::Text {
            id: p.id,
            text: p.text.unwrap_or_default(),
        },
    }
}

/// A session doc handle: typed access over a LoroDoc with the schema above.
pub struct SessionDoc {
    doc: LoroDoc,
}

impl SessionDoc {
    /// Wrap an existing doc (e.g. imported from a snapshot).
    pub fn from_doc(doc: LoroDoc) -> Self {
        Self { doc }
    }

    /// Create + initialize a fresh doc for `chat_id` (host-only).
    pub fn init(chat_id: &str) -> Result<Self, DocError> {
        let doc = LoroDoc::new();
        let meta = doc.get_map("meta");
        meta.insert("chatId", chat_id)?;
        meta.insert("schemaVersion", SESSION_SCHEMA_VERSION as i64)?;
        doc.commit();
        Ok(Self { doc })
    }

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    pub fn chat_id(&self) -> Option<String> {
        match self.doc.get_map("meta").get("chatId") {
            Some(loro::ValueOrContainer::Value(LoroValue::String(s))) => Some(s.to_string()),
            _ => None,
        }
    }

    pub fn provider_switched(&self) -> bool {
        matches!(
            self.doc.get_map("meta").get("providerSwitched"),
            Some(loro::ValueOrContainer::Value(LoroValue::Bool(true)))
        )
    }

    pub fn mark_provider_switched(&self) -> Result<(), DocError> {
        self.doc.get_map("meta").insert("providerSwitched", true)?;
        self.doc.commit();
        Ok(())
    }

    pub fn active_provider(&self) -> Option<String> {
        match self.doc.get_map("meta").get("activeProvider") {
            Some(loro::ValueOrContainer::Value(LoroValue::String(value))) => {
                Some(value.to_string())
            }
            _ => None,
        }
    }

    pub fn set_active_provider(&self, harness: &str) -> Result<(), DocError> {
        if self.active_provider().as_deref() == Some(harness) {
            return Ok(());
        }
        self.doc.get_map("meta").insert("activeProvider", harness)?;
        self.doc.commit();
        Ok(())
    }

    pub fn provider_sessions(&self) -> BTreeMap<String, ProviderSessionMark> {
        let Some(loro::ValueOrContainer::Value(LoroValue::String(value))) =
            self.doc.get_map("meta").get("providerSessions")
        else {
            return BTreeMap::new();
        };
        serde_json::from_str(&value).unwrap_or_default()
    }

    pub fn provider_session(&self, harness: &str) -> Option<ProviderSessionMark> {
        self.provider_sessions().get(harness).cloned()
    }

    pub fn set_provider_session(
        &self,
        harness: &str,
        mark: &ProviderSessionMark,
    ) -> Result<(), DocError> {
        let mut sessions = self.provider_sessions();
        if sessions.get(harness) == Some(mark) {
            return Ok(());
        }
        sessions.insert(harness.to_string(), mark.clone());
        self.doc
            .get_map("meta")
            .insert("providerSessions", serde_json::to_string(&sessions)?)?;
        self.doc.commit();
        Ok(())
    }

    /// Insert a complete message entry (user/system messages, command-side inserts).
    /// Streaming assistant entries go through [`SegmentWriter`].
    pub fn push_message(&self, entry: &SessionMessageEntry) -> Result<(), DocError> {
        let messages = self.doc.get_list("messages");
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(&map, entry)?;
        let parts = map.insert_container("parts", LoroList::new())?;
        for part in &entry.parts {
            push_part(&parts, part)?;
        }
        self.doc.commit();
        Ok(())
    }

    /// Truncate all entries from `from_index` (inclusive) to end of messages list.
    /// Used when editing a prompt to erase the prompt and all its subsequent responses.
    pub fn truncate_messages_from(&self, from_index: usize) -> Result<(), DocError> {
        let messages = self.doc.get_list("messages");
        let len = messages.len();
        if from_index < len {
            messages.delete(from_index, len - from_index)?;
            self.doc.commit();
        }
        Ok(())
    }

    /// Read all entries (continuations NOT joined — see `join_continuation_entries`).
    ///
    /// Malformed entries are SKIPPED, not fatal: a torn intermediate state
    /// (an entry map imported before the update that fills its fields) or a
    /// peer on a newer schema must degrade to a missing row, never blank the
    /// whole transcript — one bad entry took down every publish for the chat
    /// (2026-07-31, "missing field `id`" during a multi-update import).
    pub fn read_entries(&self) -> Result<Vec<SessionMessageEntry>, DocError> {
        // Materialize only the messages container — a whole-doc deep value
        // here also serialized the commands ledger on every 120ms commit tick.
        let messages = self
            .doc
            .get_list("messages")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(messages)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match entry_from_json(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(
                        chat = %self.chat_id().unwrap_or_default(),
                        error = %err,
                        "skipping unsalvageable transcript entry"
                    );
                    None
                }
            })
            .collect())
    }

    /// Read the commands ledger.
    ///
    /// Same skip-not-fail policy as `read_entries`: any device can append
    /// here, and one malformed entry must not wedge command draining for the
    /// chat forever (an unparseable command can't be executed anyway).
    pub fn read_commands(&self) -> Result<Vec<SessionCommandEntry>, DocError> {
        // Container-scoped for the same reason as `read_entries`: the drain
        // loop runs this per tick and must not pay for the transcript.
        let commands = self
            .doc
            .get_list("commands")
            .get_deep_value()
            .to_json_value();
        let raw: Vec<serde_json::Value> = serde_json::from_value(commands)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| match serde_json::from_value(v) {
                Ok(entry) => Some(entry),
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed command entry");
                    None
                }
            })
            .collect())
    }

    /// Append a command entry (rule 1: own entries only, append-only).
    pub fn queue_command(&self, entry: &SessionCommandEntry) -> Result<(), DocError> {
        let commands = self.doc.get_list("commands");
        let map = commands.push_container(LoroMap::new())?;
        map.insert("id", entry.id.as_str())?;
        map.insert(
            "kind",
            serde_json::to_value(entry.kind())?
                .as_str()
                .ok_or_else(|| DocError::Schema("kind not a string".into()))?,
        )?;
        map.insert(
            "payload",
            loro_value_from_json(&serde_json::to_value(&entry.payload)?),
        )?;
        map.insert("issuedBy", entry.issued_by.as_str())?;
        map.insert("issuedAt", entry.issued_at)?;
        if let Some(based_on) = &entry.based_on {
            map.insert(
                "basedOn",
                loro_value_from_json(&serde_json::to_value(based_on)?),
            )?;
        }
        if let Some(expires_at) = entry.expires_at {
            map.insert("expiresAt", expires_at)?;
        }
        map.insert(
            "status",
            serde_json::to_value(entry.status)?
                .as_str()
                .ok_or_else(|| DocError::Schema("status not a string".into()))?,
        )?;
        self.doc.commit();
        Ok(())
    }

    /// Rule 2: host (or the issuing composer, for `cancelled`) writes an outcome.
    pub fn set_command_status(
        &self,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<(), DocError> {
        let commands = self.doc.get_list("commands");
        for i in 0..commands.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                commands.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == command_id
                );
                if id_matches {
                    map.insert(
                        "status",
                        serde_json::to_value(status)?
                            .as_str()
                            .ok_or_else(|| DocError::Schema("status not a string".into()))?,
                    )?;
                    if let Some(r) = resolution {
                        map.insert("resolution", r)?;
                    }
                    self.doc.commit();
                    return Ok(());
                }
            }
        }
        Err(DocError::Schema(format!("command {command_id} not found")))
    }

    /// Stamp a terminal status on an existing message entry by id (recovery:
    /// abandoned `streaming` entries from a dead run are stamped `aborted`).
    /// Returns `false` when no entry with that id exists.
    pub fn set_message_status(
        &self,
        message_id: &str,
        status: MessageStatus,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            if let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) =
                messages.get(i)
            {
                let id_matches = matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
                );
                if id_matches {
                    map.insert("status", status_str(status))?;
                    self.doc.commit();
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Append an error part to an existing entry (crash recovery: the aborted
    /// entry must SAY why it ended — "Run interrupted by engine restart…" —
    /// not just truncate silently). Returns `false` when no entry matches.
    pub fn append_error_part(
        &self,
        message_id: &str,
        part_id: &str,
        message: &str,
    ) -> Result<bool, DocError> {
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let id_matches = matches!(
                entry.get("id"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
            );
            if !id_matches {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            // Idempotent per part id (recovery may re-run on a crash loop).
            for j in 0..parts.len() {
                if let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                    && matches!(
                        part.get("id"),
                        Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == part_id
                    )
                {
                    return Ok(true);
                }
            }
            push_part(
                &parts,
                &MessagePart::Error {
                    id: part_id.to_string(),
                    message: message.to_string(),
                    kind: komet_proto::ErrorKind::Unknown,
                    detail: None,
                    retryable: false,
                },
            )?;
            self.doc.commit();
            return Ok(true);
        }
        Ok(false)
    }

    pub fn append_switch_part(
        &self,
        message_id: &str,
        part: &MessagePart,
    ) -> Result<bool, DocError> {
        if !matches!(part, MessagePart::Switch { .. }) {
            return Err(DocError::Schema(
                "append_switch_part requires a switch part".into(),
            ));
        }
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let id_matches = matches!(
                entry.get("id"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == message_id
            );
            if !id_matches {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                return Err(DocError::Schema(format!(
                    "message {message_id} has no parts list"
                )));
            };
            if (0..parts.len()).any(|index| {
                matches!(
                    parts.get(index),
                    Some(loro::ValueOrContainer::Container(loro::Container::Map(part_map)))
                        if matches!(
                            part_map.get("id"),
                            Some(loro::ValueOrContainer::Value(LoroValue::String(id))) if id.as_str() == part.id()
                        )
                )
            }) {
                return Ok(true);
            }
            push_part(&parts, part)?;
            self.doc.commit();
            return Ok(true);
        }
        Ok(false)
    }

    /// Persist a compaction fact that arrived OUTSIDE a running turn (an
    /// auto-compaction between two prompts): create its own `System` entry, or
    /// refine the part already in the doc.
    ///
    /// One entry per cycle (`sys-cmp-{id}`), appended at the tail: doc order —
    /// not `created_at` — is the transcript order, and inter-turn facts arrive
    /// in order, so the tail is the right position. Idempotent per cycle: a
    /// later frame of the same compaction updates the part that exists instead
    /// of stacking a second entry.
    pub fn record_compaction_part(
        &self,
        part: &MessagePart,
        device_id: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        let MessagePart::Compaction { id, .. } = part else {
            return Err(DocError::Schema(
                "record_compaction_part requires a compaction part".into(),
            ));
        };
        if let Some(map) = self.find_part_map("compaction", id, id) {
            update_part_fields(&map, part)?;
            self.doc.commit();
            return Ok(());
        }
        let entry_id = format!("sys-{id}");
        let str_field = |value: Option<loro::ValueOrContainer>, key: &str| {
            matches!(
                value,
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == key
            )
        };
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            if !str_field(entry.get("id"), &entry_id) {
                continue;
            }
            if let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            {
                push_part(&parts, part)?;
                self.doc.commit();
                return Ok(());
            }
        }
        self.push_message(&SessionMessageEntry {
            id: entry_id,
            role: MessageRole::System,
            parts: vec![part.clone()],
            created_at,
            device_id: device_id.to_string(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Stamp the post-compaction token count that the boundary frame itself did
    /// not carry onto an existing compaction part. First writer wins: a count
    /// the agent reported is never overwritten by an inferred one.
    pub fn apply_compaction_tokens_after(
        &self,
        part_id: &str,
        tokens_after: u64,
    ) -> Result<bool, DocError> {
        let Some(map) = self.find_part_map("compaction", part_id, part_id) else {
            return Ok(false);
        };
        if map.get("tokensAfter").is_some() {
            return Ok(false);
        }
        map.insert("tokensAfter", tokens_after as i64)?;
        self.doc.commit();
        Ok(true)
    }

    /// Recovery sweep for a compaction whose run died mid-flight: an
    /// `inProgress` part on this device's entries becomes `failed` with `note`
    /// as its error. Another device's live run owns its own parts, so its
    /// entries are left alone. Idempotent — a settled cycle is never reopened
    /// nor re-stamped. Returns how many parts were stamped.
    pub fn fail_interrupted_compactions(
        &self,
        device_id: &str,
        note: &str,
    ) -> Result<usize, DocError> {
        let messages = self.doc.get_list("messages");
        let mut stamped = 0usize;
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let owned = matches!(
                entry.get("deviceId"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == device_id
            );
            if !owned {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                if !matches!(
                    part.get("kind"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "compaction"
                ) {
                    continue;
                }
                if !matches!(
                    part.get("compactionStatus"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "inProgress"
                ) {
                    continue;
                }
                part.insert("compactionStatus", "failed")?;
                part.insert("compactionError", note)?;
                stamped += 1;
            }
        }
        if stamped > 0 {
            self.doc.commit();
        }
        Ok(stamped)
    }

    /// Recovery sweep for a subagent whose run died mid-flight: a spawn chip
    /// still `running` on this device's entries becomes `failed`. The child's
    /// transcript stays reconstructible (it lives in the host's run journal,
    /// not the doc), but its terminal numbers are not: the journal carries no
    /// timestamps, so the chip keeps `subagentDurationMs` absent rather than
    /// inventing a duration, and the tool count is simply whatever the run had
    /// committed before it died — no count is written here. Another device's
    /// live run owns its own chips, so its entries are left alone. Idempotent.
    pub fn fail_running_subagents(&self, device_id: &str) -> Result<usize, DocError> {
        let messages = self.doc.get_list("messages");
        let mut stamped = 0usize;
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let owned = matches!(
                entry.get("deviceId"),
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == device_id
            );
            if !owned {
                continue;
            }
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                if !matches!(
                    part.get("kind"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "tool"
                ) {
                    continue;
                }
                if !matches!(
                    part.get("subagentStatus"),
                    Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == "running"
                ) {
                    continue;
                }
                part.insert("subagentStatus", "failed")?;
                stamped += 1;
            }
        }
        if stamped > 0 {
            self.doc.commit();
        }
        Ok(stamped)
    }

    /// engine routes a boundary frame by it: a cycle that began between two
    /// turns has its own `System` entry, and a later frame must refine THAT part
    /// instead of minting a twin inside the next assistant segment.
    pub fn has_compaction_part(&self, part_id: &str) -> bool {
        self.find_part_map("compaction", part_id, part_id).is_some()
    }

    /// Mark the input part carrying `request_id` resolved, wherever it lives
    /// (input parts store the request id as their part id). The live-run path
    /// resolves through the entry fold; this direct write is for answers to a
    /// question whose run already died — no fold owns the entry anymore.
    /// It records no answer: use [`Self::record_input_response`] for that.
    /// Returns `false` when no such part exists.
    pub fn resolve_input(&self, request_id: &str) -> Result<bool, DocError> {
        let Some(part) = self.find_part_map("input", request_id, request_id) else {
            return Ok(false);
        };
        part.insert("resolved", true)?;
        self.doc.commit();
        Ok(true)
    }

    /// The one writer of a question's answers: append `response` to the part's
    /// `responses` list and mark it resolved. Used whether or not a run is
    /// live, so an answer is recorded exactly the same way in both cases.
    ///
    /// Idempotent per device — a retried command re-stamps `resolved` but never
    /// adds the same device's answer twice. Every other device's answer stays
    /// visible: the list is append-only, and the first entry in doc order is
    /// what the transcript shows, so a concurrent answer becomes a rendered
    /// conflict instead of an overwrite. `applied` is the caller's (the host
    /// knows what it actually delivered); nothing here derives it from position.
    /// Returns `false` when no such part exists.
    pub fn record_input_response(
        &self,
        request_id: &str,
        response: &InputResponse,
    ) -> Result<bool, DocError> {
        let Some(part) = self.find_part_map("input", request_id, request_id) else {
            return Ok(false);
        };
        // The doc replicates, so a secret answer must never be stored as typed
        // (the harness already received the real one through its resolver) and
        // free text is bounded before it becomes transcript content. This
        // writer runs on every device — the answering one, a peer replaying
        // the command, a recovered run — so this is the one gate that can
        // guarantee both.
        let questions: Vec<komet_proto::UserInputQuestion> = part
            .get("questions")
            .map(|value| value.get_deep_value().to_json_value())
            .and_then(|json| serde_json::from_value(json).ok())
            .unwrap_or_default();
        let response = &sanitize_input_response(&questions, response);
        let responses: LoroList = match part.get("responses") {
            Some(loro::ValueOrContainer::Container(loro::Container::List(list))) => list,
            _ => part.insert_container("responses", LoroList::new())?,
        };
        let already_recorded = (0..responses.len()).any(|ix| {
            matches!(
                responses.get(ix),
                Some(loro::ValueOrContainer::Container(loro::Container::Map(existing)))
                    if matches!(
                        existing.get("device"),
                        Some(loro::ValueOrContainer::Value(LoroValue::String(s)))
                            if s.as_str() == response.device
                    )
            )
        });
        if !already_recorded {
            let value = serde_json::to_value(response)?;
            push_response(&responses, &value)?;
        }
        part.insert("resolved", true)?;
        self.doc.commit();
        Ok(true)
    }

    /// Mark the permission part carrying `request_id` resolved, recording no
    /// decision. See [`Self::decide_permission`] for a decision that must
    /// survive in the doc.
    pub fn resolve_permission(&self, request_id: &str) -> Result<bool, DocError> {
        self.decide_permission(request_id, None, None, None)
    }

    /// The one writer of a permission outcome: stamp `resolved`, the `decision`,
    /// and who/when decided it. The live run records the decision through the
    /// fold; this direct write is what keeps a decision made with no live run
    /// (engine restart, watchdog, another device) from vanishing.
    ///
    /// Deny wins: a part already decided Deny is left alone, so a late Allow
    /// from a second device can never rewrite what the agent was given.
    /// Returns `false` when no such part exists.
    pub fn decide_permission(
        &self,
        request_id: &str,
        decision: Option<&komet_proto::PermissionChoice>,
        decided_by: Option<&str>,
        decided_at: Option<i64>,
    ) -> Result<bool, DocError> {
        let perm_id = format!("perm-{request_id}");
        let Some(part) = self.find_part_map("permission", &perm_id, request_id) else {
            return Ok(false);
        };
        let already_denied = part
            .get("decision")
            .map(|value| value.get_deep_value().to_json_value())
            .and_then(|json| serde_json::from_value::<komet_proto::PermissionChoice>(json).ok())
            .is_some_and(|current| current == komet_proto::PermissionChoice::Deny);
        if !already_denied {
            if let Some(decision) = decision {
                let value = serde_json::to_value(decision)?;
                part.insert("decision", loro_value_from_json(&value))?;
            }
            if let Some(decided_by) = decided_by {
                part.insert("decidedBy", decided_by)?;
            }
            if let Some(decided_at) = decided_at {
                part.insert("decidedAt", decided_at)?;
            }
        }
        part.insert("resolved", true)?;
        self.doc.commit();
        Ok(true)
    }

    /// Locate one part map by kind, matching its `id` column or (for rows that
    /// only carry it there) its `requestId` column.
    fn find_part_map(&self, kind: &str, part_id: &str, request_id: &str) -> Option<LoroMap> {
        let str_field = |value: Option<loro::ValueOrContainer>, key: &str| {
            matches!(
                value,
                Some(loro::ValueOrContainer::Value(LoroValue::String(s))) if s.as_str() == key
            )
        };
        let messages = self.doc.get_list("messages");
        for i in 0..messages.len() {
            let Some(loro::ValueOrContainer::Container(loro::Container::Map(entry))) =
                messages.get(i)
            else {
                continue;
            };
            let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                entry.get("parts")
            else {
                continue;
            };
            for j in 0..parts.len() {
                let Some(loro::ValueOrContainer::Container(loro::Container::Map(part))) =
                    parts.get(j)
                else {
                    continue;
                };
                if !str_field(part.get("kind"), kind) {
                    continue;
                }
                if str_field(part.get("id"), part_id)
                    || str_field(part.get("requestId"), request_id)
                {
                    return Some(part);
                }
            }
        }
        None
    }

    /// Export a snapshot (persistence) — `ExportMode::Snapshot`.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, DocError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocError::Schema(e.to_string()))
    }
}

fn write_entry_scalar_fields(map: &LoroMap, entry: &SessionMessageEntry) -> Result<(), DocError> {
    map.insert("id", entry.id.as_str())?;
    map.insert(
        "role",
        match entry.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        },
    )?;
    map.insert("createdAt", entry.created_at)?;
    map.insert("deviceId", entry.device_id.as_str())?;
    if let Some(status) = entry.status {
        map.insert("status", status_str(status))?;
    }
    if let Some(continuation_of) = &entry.continuation_of {
        map.insert("continuationOf", continuation_of.as_str())?;
    }
    Ok(())
}

fn status_str(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Streaming => "streaming",
        MessageStatus::Complete => "complete",
        MessageStatus::Aborted => "aborted",
    }
}

/// Append one part map to a parts list; text bodies become LoroText containers.
fn push_part(parts: &LoroList, part: &MessagePart) -> Result<(), DocError> {
    let map = parts.push_container(LoroMap::new())?;
    let doc_part = to_doc_part(part)?;
    map.insert("id", doc_part.id.as_str())?;
    map.insert("kind", doc_part.kind.as_str())?;
    if let Some(text) = &doc_part.text {
        let t = map.insert_container("text", LoroText::new())?;
        t.insert(0, text)?;
    }
    if let Some(call) = &doc_part.call {
        map.insert("call", loro_value_from_json(call))?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(output) = &doc_part.output {
        map.insert("output", output.as_str())?;
    }
    if let Some(diff) = &doc_part.diff {
        map.insert("diff", loro_value_from_json(diff))?;
    }
    if let Some(output_ref) = &doc_part.output_ref {
        map.insert("outputRef", output_ref.as_str())?;
    }
    if let Some(output_bytes) = doc_part.output_bytes {
        map.insert("outputBytes", output_bytes as i64)?;
    }
    if let Some(diff_ref) = &doc_part.diff_ref {
        map.insert("diffRef", diff_ref.as_str())?;
    }
    if let Some(diff_stats) = &doc_part.diff_stats {
        map.insert("diffStats", loro_value_from_json(diff_stats))?;
    }
    if let Some(subagent_ref) = &doc_part.subagent_ref {
        map.insert("subagentRef", subagent_ref.as_str())?;
    }
    if let Some(subagent_status) = &doc_part.subagent_status {
        map.insert("subagentStatus", loro_value_from_json(subagent_status))?;
    }
    if let Some(subagent_tail) = &doc_part.subagent_tail {
        map.insert("subagentTail", subagent_tail.as_str())?;
    }
    if let Some(subagent_duration_ms) = doc_part.subagent_duration_ms {
        map.insert("subagentDurationMs", subagent_duration_ms as i64)?;
    }
    if let Some(subagent_tool_count) = doc_part.subagent_tool_count {
        map.insert("subagentToolCount", subagent_tool_count as i64)?;
    }
    if let Some(error_kind) = &doc_part.error_kind {
        map.insert(
            "errorKind",
            loro_value_from_json(&serde_json::to_value(error_kind)?),
        )?;
    }
    if let Some(error_detail) = &doc_part.error_detail {
        map.insert("errorDetail", error_detail.as_str())?;
    }
    if let Some(error_retryable) = doc_part.error_retryable {
        map.insert("errorRetryable", error_retryable)?;
    }
    if let Some(request_id) = &doc_part.request_id {
        map.insert("requestId", request_id.as_str())?;
    }
    if let Some(decision) = &doc_part.decision {
        map.insert(
            "decision",
            loro_value_from_json(&serde_json::to_value(decision)?),
        )?;
    }
    if let Some(tool_call_id) = &doc_part.tool_call_id {
        map.insert("toolCallId", tool_call_id.as_str())?;
    }
    if let Some(decided_by) = &doc_part.decided_by {
        map.insert("decidedBy", decided_by.as_str())?;
    }
    if let Some(decided_at) = doc_part.decided_at {
        map.insert("decidedAt", decided_at)?;
    }
    if let Some(source) = doc_part.source {
        map.insert(
            "source",
            loro_value_from_json(&serde_json::to_value(source)?),
        )?;
    }
    write_compaction_fields(&map, &doc_part)?;
    // Every input part gets its `responses` container up front, even when
    // empty: a container inserted concurrently on two peers under the same
    // key is a conflict where one list is dropped. Materialising it in the
    // history means every device appends to the same list, and two offline
    // answers merge instead of one vanishing (display winner = position 0).
    if doc_part.kind == "input" {
        let list = map.insert_container("responses", LoroList::new())?;
        for response in doc_part
            .responses
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            push_response(&list, response)?;
        }
    }
    Ok(())
}

/// Write one answer record into a part's `responses` list.
fn push_response(list: &LoroList, response: &serde_json::Value) -> Result<(), DocError> {
    let map = list.push_container(LoroMap::new())?;
    if let Some(answers) = response.get("answers") {
        map.insert("answers", loro_value_from_json(answers))?;
    }
    if let Some(decided_at) = response
        .get("decidedAt")
        .and_then(serde_json::Value::as_i64)
    {
        map.insert("decidedAt", decided_at)?;
    }
    if let Some(device) = response.get("device").and_then(serde_json::Value::as_str) {
        map.insert("device", device)?;
    }
    if response.get("applied").and_then(serde_json::Value::as_bool) == Some(true) {
        map.insert("applied", true)?;
    }
    Ok(())
}

/// The answer as it may be stored: a secret field's content is replaced with
/// [`komet_proto::WITHHELD_SECRET_ANSWER`] (labels cleared too — the composer
/// mirrors typed text into both), and every remaining string is capped.
fn sanitize_input_response(
    questions: &[komet_proto::UserInputQuestion],
    response: &InputResponse,
) -> InputResponse {
    let mut response = response.clone();
    for answer in &mut response.answers {
        let secret = questions
            .iter()
            .any(|question| question.id == answer.question_id && question.secret);
        let answered = !answer.labels.is_empty() || answer.free_text.is_some();
        if secret && answered {
            answer.labels.clear();
            answer.free_text = Some(komet_proto::WITHHELD_SECRET_ANSWER.to_owned());
            continue;
        }
        for label in &mut answer.labels {
            *label = cap_free_text(label);
        }
        if let Some(text) = &mut answer.free_text {
            *text = cap_free_text(text);
        }
    }
    response
}

/// Truncate at a char boundary inside [`komet_proto::USER_INPUT_TEXT_CAP`],
/// ending with an ellipsis so a cut answer reads as cut.
fn cap_free_text(text: &str) -> String {
    if text.len() <= komet_proto::USER_INPUT_TEXT_CAP {
        return text.to_owned();
    }
    let marker = "…";
    let mut end = komet_proto::USER_INPUT_TEXT_CAP - marker.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

fn entry_from_json(v: serde_json::Value) -> Result<SessionMessageEntry, DocError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawEntry {
        id: String,
        role: MessageRole,
        #[serde(default)]
        parts: Vec<DocPartJson>,
        created_at: i64,
        device_id: String,
        #[serde(default)]
        status: Option<MessageStatus>,
        #[serde(default)]
        continuation_of: Option<String>,
    }
    match serde_json::from_value::<RawEntry>(v.clone()) {
        Ok(raw) => Ok(SessionMessageEntry {
            id: raw.id,
            role: raw.role,
            parts: raw.parts.into_iter().map(from_doc_part).collect(),
            created_at: raw.created_at,
            device_id: raw.device_id,
            status: raw.status,
            continuation_of: raw.continuation_of,
        }),
        // 2026-08-10 incident rule: a missing field must cost AT MOST what
        // the field carried — never the entry, never the transcript. Rooms
        // merge writes from every device and app version; one bad writer
        // (or one mangled export) blanking whole sessions for every reader
        // is exactly what tonight looked like.
        Err(strict_err) => salvage_entry(v, strict_err),
    }
}

/// Field-level salvage for entries the strict shape rejects. Missing
/// identity/attribution fields get deterministic stand-ins (content-hashed
/// id, so repeated reads and continuation joins stay stable); parts are
/// salvaged individually — a part missing `kind` is inferred from its
/// content shape, and only truly contentless parts are dropped.
fn salvage_entry(
    v: serde_json::Value,
    strict_err: serde_json::Error,
) -> Result<SessionMessageEntry, DocError> {
    let Some(obj) = v.as_object() else {
        return Err(DocError::Json(strict_err));
    };
    let stable_hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        v.to_string().hash(&mut hasher);
        hasher.finish()
    };
    let str_field = |key: &str| obj.get(key).and_then(|x| x.as_str()).map(str::to_owned);
    let id = str_field("id").unwrap_or_else(|| format!("recovered-{stable_hash:016x}"));
    let role = obj
        .get("role")
        .and_then(|r| serde_json::from_value::<MessageRole>(r.clone()).ok())
        .unwrap_or(MessageRole::Assistant);
    let mut parts = Vec::new();
    let mut dropped_parts = 0usize;
    if let Some(raw_parts) = obj.get("parts").and_then(|p| p.as_array()) {
        for (ix, part) in raw_parts.iter().enumerate() {
            match serde_json::from_value::<DocPartJson>(part.clone()) {
                Ok(p) => parts.push(from_doc_part(p)),
                Err(_) => match salvage_part(part, &id, ix) {
                    Some(p) => parts.push(p),
                    None => dropped_parts += 1,
                },
            }
        }
    }
    tracing::warn!(
        entry = %id,
        error = %strict_err,
        salvaged_parts = parts.len(),
        dropped_parts,
        "transcript entry failed strict parse; salvaged"
    );
    Ok(SessionMessageEntry {
        id,
        role,
        parts,
        created_at: obj.get("createdAt").and_then(|x| x.as_i64()).unwrap_or(0),
        device_id: str_field("deviceId").unwrap_or_default(),
        status: obj
            .get("status")
            .and_then(|s| serde_json::from_value(s.clone()).ok()),
        continuation_of: str_field("continuationOf"),
    })
}

/// Salvage one part whose strict `DocPartJson` parse failed: infer the kind
/// from the content shape (`text` → text part, parseable `call` → tool
/// part). `None` only when nothing renderable survives.
fn salvage_part(part: &serde_json::Value, entry_id: &str, ix: usize) -> Option<MessagePart> {
    let obj = part.as_object()?;
    let id = obj
        .get("id")
        .and_then(|x| x.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{entry_id}#recovered-{ix}"));
    // A compaction boundary is recognized by its OWN columns, before the text
    // heuristic: a row whose summary size is malformed must still show that the
    // agent squeezed the history, or the transcript would silently claim an
    // unbroken conversation.
    if obj.get("kind").and_then(|x| x.as_str()) == Some("compaction") {
        let str_field = |key: &str| obj.get(key).and_then(|x| x.as_str()).map(str::to_owned);
        let num_field = |key: &str| obj.get(key).and_then(|x| x.as_u64());
        let compaction_id = str_field("compactionId")
            .unwrap_or_else(|| id.strip_prefix("cmp-").unwrap_or(id.as_str()).to_owned());
        return Some(MessagePart::Compaction {
            id,
            compaction_id,
            status: obj
                .get("compactionStatus")
                .and_then(|s| serde_json::from_value(s.clone()).ok())
                .unwrap_or_default(),
            trigger: str_field("trigger"),
            tokens_before: num_field("tokensBefore"),
            tokens_after: num_field("tokensAfter"),
            error: str_field("compactionError"),
            summary: str_field("summary"),
            summary_bytes: num_field("summaryBytes"),
            summary_in_journal: obj
                .get("summaryInJournal")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        });
    }
    if let Some(text) = obj.get("text").and_then(|x| x.as_str()) {
        return Some(MessagePart::Text {
            id,
            text: text.to_owned(),
        });
    }
    if let Some(call) = obj
        .get("call")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
    {
        return Some(MessagePart::Tool {
            id,
            call,
            is_error: obj
                .get("isError")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            resolved: obj
                .get("resolved")
                .and_then(|x| x.as_bool())
                .unwrap_or(true),
            output: obj
                .get("output")
                .and_then(|x| x.as_str())
                .map(str::to_owned),
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
    if let Some(message) = obj.get("message").and_then(|x| x.as_str()) {
        return Some(MessagePart::Error {
            id,
            message: message.to_owned(),
            kind: komet_proto::ErrorKind::Unknown,
            detail: None,
            retryable: false,
        });
    }
    None
}

/// Render-time continuation join at the entry level (`joinContinuations` in TS):
/// concatenate continuation entries' parts onto their root, in list order.
pub fn join_continuation_entries(entries: Vec<SessionMessageEntry>) -> Vec<SessionMessageEntry> {
    if !entries.iter().any(|e| e.continuation_of.is_some()) {
        return entries;
    }
    let mut out: Vec<SessionMessageEntry> = Vec::with_capacity(entries.len());
    let mut root_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in entries {
        match &entry.continuation_of {
            Some(root_id) => {
                if let Some(&at) = root_index.get(root_id) {
                    out[at].parts.extend(entry.parts);
                } else {
                    // Orphan continuation — surface as its own entry rather than dropping.
                    out.push(entry);
                }
            }
            None => {
                root_index.insert(entry.id.clone(), out.len());
                out.push(entry);
            }
        }
    }
    out
}

/// Incremental streaming writer for one assistant entry.
///
/// Port of komet's `DocSegmentWriter` diff discipline: called with the *folded* parts of the
/// live segment (from `fold_event_into_parts`) at each commit tick, it diffs against what's in
/// the doc and writes only the delta:
/// - trailing text growth → `LoroText` append (RLE-merged),
/// - new parts → pushed,
/// - tool call refresh / resolution / input resolution → in-place map updates.
///
/// Invariant relied upon: the fold only ever APPENDS parts or grows the trailing text; earlier
/// text never mutates. Tool/input parts may update fields in place.
pub struct SegmentWriter<'a> {
    doc: &'a SessionDoc,
    /// Index of this entry in the `messages` list.
    entry_index: usize,
    /// Mirror of what we've written so far (part id → app part).
    written: Vec<MessagePart>,
}

impl<'a> SegmentWriter<'a> {
    /// Begin a streaming assistant entry: pushes the entry with `status: streaming`, no parts.
    pub fn begin(
        doc: &'a SessionDoc,
        entry_id: &str,
        device_id: &str,
        created_at: i64,
    ) -> Result<Self, DocError> {
        let messages = doc.doc.get_list("messages");
        let entry_index = messages.len();
        let map = messages.push_container(LoroMap::new())?;
        write_entry_scalar_fields(
            &map,
            &SessionMessageEntry {
                id: entry_id.into(),
                role: MessageRole::Assistant,
                parts: vec![],
                created_at,
                device_id: device_id.into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            },
        )?;
        map.insert_container("parts", LoroList::new())?;
        doc.doc.commit();
        Ok(Self {
            doc,
            entry_index,
            written: Vec::new(),
        })
    }

    fn entry_map(&self) -> Result<LoroMap, DocError> {
        let messages = self.doc.doc.get_list("messages");
        match messages.get(self.entry_index) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
            _ => Err(DocError::Schema("streaming entry map missing".into())),
        }
    }

    fn parts_list(&self) -> Result<LoroList, DocError> {
        match self.entry_map()?.get("parts") {
            Some(loro::ValueOrContainer::Container(loro::Container::List(list))) => Ok(list),
            _ => Err(DocError::Schema(
                "streaming entry parts list missing".into(),
            )),
        }
    }

    /// Diff `folded` (the full folded segment so far) into the doc.
    pub fn sync(&mut self, folded: &[MessagePart]) -> Result<(), DocError> {
        let parts = self.parts_list()?;
        let mut dirty = false;

        for (i, part) in folded.iter().enumerate() {
            match self.written.get(i) {
                None => {
                    push_part(&parts, part)?;
                    self.written.push(part.clone());
                    dirty = true;
                }
                Some(prev) if prev == part => {}
                Some(prev) => {
                    match (prev, part) {
                        (
                            MessagePart::Text { text: old, .. },
                            MessagePart::Text { text: new, .. },
                        ) if new.starts_with(old.as_str()) => {
                            // Trailing-text growth: append the suffix into the LoroText.
                            let delta = &new[old.len()..];
                            if !delta.is_empty() {
                                let part_map = part_map_at(&parts, i)?;
                                match part_map.get("text") {
                                    Some(loro::ValueOrContainer::Container(
                                        loro::Container::Text(t),
                                    )) => {
                                        let len = t.len_unicode();
                                        t.insert(len, delta)?;
                                    }
                                    _ => {
                                        return Err(DocError::Schema(
                                            "text part missing LoroText".into(),
                                        ));
                                    }
                                }
                                dirty = true;
                            }
                        }
                        _ => {
                            // Field-level update (tool refresh/resolve, input resolve, or a
                            // non-append text rewrite, which the fold shouldn't produce —
                            // rewrite the part map fields defensively).
                            let part_map = part_map_at(&parts, i)?;
                            update_part_fields(&part_map, part)?;
                            dirty = true;
                        }
                    }
                    self.written[i] = part.clone();
                }
            }
        }

        if dirty {
            self.doc.doc.commit();
        }
        Ok(())
    }

    /// Finish the stream: sync final parts and stamp a terminal status.
    pub fn finish(mut self, folded: &[MessagePart], status: MessageStatus) -> Result<(), DocError> {
        self.sync(folded)?;
        let map = self.entry_map()?;
        map.insert("status", status_str(status))?;
        self.doc.doc.commit();
        Ok(())
    }
}

fn part_map_at(parts: &LoroList, index: usize) -> Result<LoroMap, DocError> {
    match parts.get(index) {
        Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
        _ => Err(DocError::Schema(format!("part map missing at {index}"))),
    }
}

/// In-place field refresh for tool/input parts (and defensive text rewrite).
fn update_part_fields(map: &LoroMap, part: &MessagePart) -> Result<(), DocError> {
    let doc_part = to_doc_part(part)?;
    if let Some(call) = &doc_part.call {
        map.insert("call", loro_value_from_json(call))?;
    }
    if let Some(is_error) = doc_part.is_error {
        map.insert("isError", is_error)?;
    }
    if let Some(questions) = &doc_part.questions {
        map.insert("questions", loro_value_from_json(questions))?;
    }
    if let Some(resolved) = doc_part.resolved {
        map.insert("resolved", resolved)?;
    }
    if let Some(message) = &doc_part.message {
        map.insert("message", message.as_str())?;
    }
    if let Some(output) = &doc_part.output {
        map.insert("output", output.as_str())?;
    }
    if let Some(diff) = &doc_part.diff {
        map.insert("diff", loro_value_from_json(diff))?;
    }
    if let Some(output_ref) = &doc_part.output_ref {
        map.insert("outputRef", output_ref.as_str())?;
    }
    if let Some(output_bytes) = doc_part.output_bytes {
        map.insert("outputBytes", output_bytes as i64)?;
    }
    if let Some(diff_ref) = &doc_part.diff_ref {
        map.insert("diffRef", diff_ref.as_str())?;
    }
    if let Some(diff_stats) = &doc_part.diff_stats {
        map.insert("diffStats", loro_value_from_json(diff_stats))?;
    }
    if let Some(subagent_ref) = &doc_part.subagent_ref {
        map.insert("subagentRef", subagent_ref.as_str())?;
    }
    if let Some(subagent_status) = &doc_part.subagent_status {
        map.insert("subagentStatus", loro_value_from_json(subagent_status))?;
    }
    if let Some(subagent_tail) = &doc_part.subagent_tail {
        map.insert("subagentTail", subagent_tail.as_str())?;
    }
    if let Some(subagent_duration_ms) = doc_part.subagent_duration_ms {
        map.insert("subagentDurationMs", subagent_duration_ms as i64)?;
    }
    if let Some(subagent_tool_count) = doc_part.subagent_tool_count {
        map.insert("subagentToolCount", subagent_tool_count as i64)?;
    }
    if let Some(error_kind) = &doc_part.error_kind {
        map.insert(
            "errorKind",
            loro_value_from_json(&serde_json::to_value(error_kind)?),
        )?;
    }
    if let Some(error_detail) = &doc_part.error_detail {
        map.insert("errorDetail", error_detail.as_str())?;
    }
    if let Some(error_retryable) = doc_part.error_retryable {
        map.insert("errorRetryable", error_retryable)?;
    }
    if let Some(request_id) = &doc_part.request_id {
        map.insert("requestId", request_id.as_str())?;
    }
    if let Some(decision) = &doc_part.decision {
        map.insert(
            "decision",
            loro_value_from_json(&serde_json::to_value(decision)?),
        )?;
    }
    if let Some(tool_call_id) = &doc_part.tool_call_id {
        map.insert("toolCallId", tool_call_id.as_str())?;
    }
    if let Some(decided_by) = &doc_part.decided_by {
        map.insert("decidedBy", decided_by.as_str())?;
    }
    if let Some(decided_at) = doc_part.decided_at {
        map.insert("decidedAt", decided_at)?;
    }
    if let Some(source) = doc_part.source {
        map.insert(
            "source",
            loro_value_from_json(&serde_json::to_value(source)?),
        )?;
    }
    write_compaction_fields(map, &doc_part)?;
    // `responses` is deliberately absent: the answer list grows through
    // [`SessionDoc::record_input_response`], which appends into the container.
    // Writing the fold's (always empty) copy here would erase real answers.
    if let Some(text) = &doc_part.text {
        // Defensive path only — the fold never rewrites earlier text.
        if let Some(loro::ValueOrContainer::Container(loro::Container::Text(t))) = map.get("text") {
            t.update(text, Default::default())
                .map_err(|e| DocError::Schema(e.to_string()))?;
        }
    }
    Ok(())
}

/// Write the compaction columns. Shared by the push and the in-place update:
/// the two field lists have drifted before (columns that existed in the struct
/// and were never written), so a new kind gets one writer, not two.
fn write_compaction_fields(map: &LoroMap, doc_part: &DocPartJson) -> Result<(), DocError> {
    if let Some(compaction_id) = &doc_part.compaction_id {
        map.insert("compactionId", compaction_id.as_str())?;
    }
    if let Some(status) = &doc_part.compaction_status {
        map.insert(
            "compactionStatus",
            loro_value_from_json(&serde_json::to_value(status)?),
        )?;
    }
    if let Some(trigger) = &doc_part.trigger {
        map.insert("trigger", trigger.as_str())?;
    }
    if let Some(tokens_before) = doc_part.tokens_before {
        map.insert("tokensBefore", tokens_before as i64)?;
    }
    if let Some(tokens_after) = doc_part.tokens_after {
        map.insert("tokensAfter", tokens_after as i64)?;
    }
    if let Some(error) = &doc_part.compaction_error {
        map.insert("compactionError", error.as_str())?;
    }
    if let Some(summary) = &doc_part.summary {
        map.insert("summary", summary.as_str())?;
    }
    if let Some(summary_bytes) = doc_part.summary_bytes {
        map.insert("summaryBytes", summary_bytes as i64)?;
    }
    if doc_part.summary_in_journal == Some(true) {
        map.insert("summaryInJournal", true)?;
    }
    Ok(())
}

fn loro_value_from_json(v: &serde_json::Value) -> LoroValue {
    LoroValue::from(v.clone())
}

/// Tail sidecar shape (`SessionTail` in TS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTail {
    pub chat_id: String,
    pub schema_version: u32,
    pub messages: Vec<SessionMessageEntry>,
    pub total_messages: usize,
    pub updated_at: i64,
}

/// Materialize the last-N joined messages (`materializeTail` in TS).
pub fn materialize_tail(
    doc: &SessionDoc,
    now: i64,
    tail_count: usize,
) -> Result<SessionTail, DocError> {
    let all = join_continuation_entries(doc.read_entries()?);
    let total = all.len();
    let start = total.saturating_sub(if tail_count == 0 {
        TAIL_MESSAGE_COUNT
    } else {
        tail_count
    });
    Ok(SessionTail {
        chat_id: doc.chat_id().unwrap_or_default(),
        schema_version: SESSION_SCHEMA_VERSION,
        messages: all[start..].to_vec(),
        total_messages: total,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::fold_event_into_parts;
    use komet_proto::{AgentEvent, ToolCall};

    fn user_entry(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn round_trips_message_entries() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m1");
        assert_eq!(
            entries[0].parts,
            vec![MessagePart::Text {
                id: "t0".into(),
                text: "hello".into()
            }]
        );
        assert_eq!(doc.chat_id().as_deref(), Some("chat-1"));
    }

    #[test]
    fn old_error_parts_reread_with_unknown_kind() {
        // 1. App-layer serde: old JSON without the new fields.
        let part: MessagePart =
            serde_json::from_str(r#"{"kind":"error","id":"e0","message":"boom"}"#).unwrap();
        assert!(
            matches!(
                &part,
                MessagePart::Error { kind, detail, retryable, .. }
                if *kind == komet_proto::ErrorKind::Unknown
                    && detail.is_none()
                    && !retryable
            ),
            "old error JSON must default to Unknown: {part:?}"
        );
        // 2. Doc-layer: old persisted part map without the new columns.
        let old = DocPartJson {
            id: "e0".into(),
            kind: "error".into(),
            message: Some("boom".into()),
            ..Default::default()
        };
        let part = from_doc_part(old);
        assert!(
            matches!(
                &part,
                MessagePart::Error { kind, detail, retryable, .. }
                if *kind == komet_proto::ErrorKind::Unknown
                    && detail.is_none()
                    && !retryable
            ),
            "old doc rows must default to Unknown: {part:?}"
        );
        // 3. Defaulted new fields stay off the wire so old readers see old bytes
        // (the enum tag itself occupies `kind`; the typed class rides `errorKind`).
        let v = serde_json::to_value(&part).unwrap();
        assert_eq!(v.get("message").unwrap(), "boom");
        assert!(v.get("errorKind").is_none());
        assert!(v.get("detail").is_none());
    }

    #[test]
    fn resolve_input_stamps_the_part_in_place() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Input {
                id: "r1".into(),
                request_id: "r1".into(),
                questions: vec![],
                resolved: false,
                responses: vec![],
                source: komet_proto::QuestionSource::Native,
            }],
            created_at: 1,
            device_id: "dev-a".into(),
            // The orphan case: the run died and recovery stamped the entry.
            status: Some(MessageStatus::Aborted),
            continuation_of: None,
        })
        .unwrap();
        assert!(!doc.resolve_input("nope").unwrap());
        assert!(doc.resolve_input("r1").unwrap());
        let entries = doc.read_entries().unwrap();
        assert!(matches!(
            &entries[0].parts[0],
            MessagePart::Input { resolved: true, .. }
        ));
    }

    fn question() -> komet_proto::UserInputQuestion {
        komet_proto::UserInputQuestion {
            id: "q1".into(),
            header: "Pick".into(),
            question: "Which one?".into(),
            options: vec!["a".into(), "b".into()],
            multi_select: false,
            option_descriptions: Vec::new(),
            allow_free_text: false,
            secret: false,
            default: None,
        }
    }

    fn answer(label: &str) -> komet_proto::UserInputAnswer {
        komet_proto::UserInputAnswer {
            question_id: "q1".into(),
            labels: vec![label.into()],
            free_text: None,
        }
    }

    fn response(device: &str, label: &str, applied: bool) -> InputResponse {
        InputResponse {
            answers: vec![answer(label)],
            decided_at: 1_700_000_000_000,
            device: device.into(),
            applied,
        }
    }

    fn assistant_with(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn question_entry(
        request_id: &str,
        source: komet_proto::QuestionSource,
    ) -> SessionMessageEntry {
        assistant_with(
            &format!("m-{request_id}"),
            vec![MessagePart::Input {
                id: request_id.into(),
                request_id: request_id.into(),
                questions: vec![question()],
                resolved: false,
                responses: vec![],
                source,
            }],
        )
    }

    /// The answers recorded on an entry's input part, in doc order.
    fn recorded(doc: &SessionDoc, entry_ix: usize) -> Vec<InputResponse> {
        let entries = doc.read_entries().unwrap();
        match &entries[entry_ix].parts[0] {
            MessagePart::Input { responses, .. } => responses.clone(),
            other => panic!("input part expected, got {other:?}"),
        }
    }

    #[test]
    fn old_input_part_without_responses_still_reads() {
        // App-layer serde: a row written before answers and sources were kept.
        let part: MessagePart = serde_json::from_str(
            r#"{"kind":"input","id":"in-r1","requestId":"r1","questions":[],"resolved":true}"#,
        )
        .unwrap();
        let MessagePart::Input {
            responses,
            source,
            resolved,
            ..
        } = &part
        else {
            panic!("input part expected");
        };
        assert!(*resolved);
        assert!(responses.is_empty(), "no answer recorded, none invented");
        assert_eq!(*source, komet_proto::QuestionSource::Native);

        // Doc-layer read: the same row with neither column in the part map.
        let read = from_doc_part(DocPartJson {
            id: "r1".into(),
            kind: "input".into(),
            questions: Some(serde_json::json!([])),
            resolved: Some(true),
            ..Default::default()
        });
        assert!(
            matches!(&read, MessagePart::Input { responses, source, .. }
                if responses.is_empty()
                    && *source == komet_proto::QuestionSource::Native),
            "old doc row must read as answered-but-unrecorded, not as an error: {read:?}"
        );
    }

    #[test]
    fn input_resolved_appends_a_response() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&question_entry("r1", komet_proto::QuestionSource::Native))
            .unwrap();
        assert!(
            doc.record_input_response("r1", &response("dev-a", "b", true))
                .unwrap()
        );
        assert!(
            !doc.record_input_response("nope", &response("dev-a", "b", true))
                .unwrap()
        );

        // A replay of the doc (snapshot → fresh handle) restates the answers.
        let bytes = doc.export_snapshot().unwrap();
        let restored = SessionDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&bytes).unwrap();
            d
        });
        let responses = recorded(&restored, 0);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].answers, vec![answer("b")]);
        assert_eq!(responses[0].device, "dev-a");
        assert!(responses[0].applied);
        assert!(
            restored.read_entries().unwrap()[0].parts[0]
                == MessagePart::Input {
                    id: "r1".into(),
                    request_id: "r1".into(),
                    questions: vec![question()],
                    resolved: true,
                    responses: responses.clone(),
                    source: komet_proto::QuestionSource::Native,
                }
        );

        // Idempotent per device: a retried answer restates `resolved`, it does
        // not add a second record for the same device.
        restored
            .record_input_response("dev-a-again-irrelevant", &response("dev-a", "a", false))
            .ok();
        restored
            .record_input_response("r1", &response("dev-a", "a", false))
            .unwrap();
        let responses = recorded(&restored, 0);
        assert_eq!(responses.len(), 1, "the same device cannot double-record");
        assert_eq!(responses[0].answers, vec![answer("b")]);
    }

    /// Split one snapshot into a replica with a deterministic peer.
    fn replica(bytes: &[u8], peer: u64) -> SessionDoc {
        let d = LoroDoc::new();
        d.set_peer_id(peer).unwrap();
        d.import(bytes).unwrap();
        SessionDoc::from_doc(d)
    }

    #[test]
    fn delivered_answer_is_not_the_merge_winner() {
        // The host delivered its own answer, but doc order elects the other
        // device's row first (an offline answer that replicated in earlier).
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&question_entry("r1", komet_proto::QuestionSource::Native))
            .unwrap();
        assert!(
            doc.record_input_response("r1", &response("dev-phone", "a", false))
                .unwrap()
        );
        assert!(
            doc.record_input_response("r1", &response("dev-host", "b", true))
                .unwrap()
        );
        let responses = recorded(&doc, 0);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].device, "dev-phone");
        assert!(
            !responses[0].applied,
            "the display winner is position-derived, not delivery-derived"
        );
        assert!(
            responses[1].applied,
            "`applied` travels with what the host actually delivered"
        );

        // A snapshot round trip restates both rows verbatim: nothing on the
        // read path promotes the winner to "delivered".
        let bytes = doc.export_snapshot().unwrap();
        let restored = replica(&bytes, 7);
        assert_eq!(recorded(&restored, 0), responses);
    }

    #[test]
    fn input_requested_carries_source() {
        // A question that arrived on the permission wire keeps that source
        // through the fold, the doc, and back.
        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::InputRequested {
                request_id: "r1".into(),
                questions: vec![question()],
                source: komet_proto::QuestionSource::PermissionShaped,
            },
        );
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&assistant_with("m1", folded)).unwrap();
        let entries = doc.read_entries().unwrap();
        let MessagePart::Input { source, id, .. } = &entries[0].parts[0] else {
            panic!("input part expected");
        };
        assert_eq!(*source, komet_proto::QuestionSource::PermissionShaped);
        // The doc keys an input part by its request id, so the round trip
        // returns that id; the fold's `in-` prefix is display-only.
        assert_eq!(id, "r1");

        // The ACP elicitation wire keeps its own source through the same
        // round trip, and says so on the wire.
        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::InputRequested {
                request_id: "r2".into(),
                questions: vec![question()],
                source: komet_proto::QuestionSource::Elicitation,
            },
        );
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&assistant_with("m2", folded)).unwrap();
        let entries = doc.read_entries().unwrap();
        let MessagePart::Input { source, .. } = &entries[0].parts[0] else {
            panic!("input part expected");
        };
        assert_eq!(*source, komet_proto::QuestionSource::Elicitation);
        let wire = serde_json::to_value(&entries[0].parts[0]).unwrap();
        assert_eq!(
            wire.get("source").and_then(|value| value.as_str()),
            Some("elicitation")
        );

        // The native channel — the historical default — adds nothing to the
        // wire, so an older reader sees the row it always saw.
        let native = serde_json::to_value(MessagePart::Input {
            id: "r2".into(),
            request_id: "r2".into(),
            questions: vec![question()],
            resolved: false,
            responses: vec![],
            source: komet_proto::QuestionSource::Native,
        })
        .unwrap();
        assert!(native.get("source").is_none(), "{native:?}");
        assert!(native.get("responses").is_none(), "{native:?}");
    }

    #[test]
    fn secret_answer_is_capped_and_not_exported() {
        // A pasted credential must not survive in state that replicates or
        // travels (the doc syncs, the export is shared). The harness still
        // gets the answer the caller handed it before this write: recording
        // redacts what it stores, not what the run already received.
        let secret = format!("ghp_{}", "K".repeat(komet_proto::USER_INPUT_TEXT_CAP));
        let pasted = "x".repeat(komet_proto::USER_INPUT_TEXT_CAP + 500);
        let mut secret_question = question();
        secret_question.id = "token".into();
        secret_question.allow_free_text = true;
        secret_question.secret = true;
        let mut notes_question = question();
        notes_question.id = "notes".into();
        notes_question.allow_free_text = true;

        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&assistant_with(
            "m1",
            vec![MessagePart::Input {
                id: "r1".into(),
                request_id: "r1".into(),
                questions: vec![secret_question, notes_question],
                resolved: false,
                responses: vec![],
                source: komet_proto::QuestionSource::Elicitation,
            }],
        ))
        .unwrap();
        let response = InputResponse {
            answers: vec![
                komet_proto::UserInputAnswer {
                    question_id: "token".into(),
                    labels: vec![secret.clone()],
                    free_text: Some(secret.clone()),
                },
                komet_proto::UserInputAnswer {
                    question_id: "notes".into(),
                    labels: vec![pasted.clone()],
                    free_text: Some(pasted.clone()),
                },
            ],
            decided_at: 1_700_000_000_000,
            device: "dev-a".into(),
            applied: true,
        };
        doc.record_input_response("r1", &response).unwrap();

        // The caller's value — what the agent's resolver received — is intact.
        assert_eq!(
            response.answers[0].free_text.as_deref(),
            Some(secret.as_str())
        );

        let entries = doc.read_entries().unwrap();
        let stored = recorded(&doc, 0);
        let token = &stored[0].answers[0];
        assert!(
            token.labels.is_empty(),
            "the mirrored label would leak the credential too: {token:?}"
        );
        assert_eq!(
            token.free_text.as_deref(),
            Some(komet_proto::WITHHELD_SECRET_ANSWER)
        );
        assert!(
            !serde_json::to_string(&entries).unwrap().contains(&secret),
            "the doc must not carry the credential"
        );

        // Free text is bounded before it becomes transcript content.
        let notes = &stored[0].answers[1];
        assert!(
            notes.labels[0].len() <= komet_proto::USER_INPUT_TEXT_CAP
                && notes.free_text.as_deref().unwrap().len() <= komet_proto::USER_INPUT_TEXT_CAP,
            "the pasted answer must be capped: {notes:?}"
        );

        // The export — the copy someone shares — has the marker, no credential.
        let md = crate::export::transcript_to_markdown(&entries, false);
        assert!(
            !md.contains(&secret),
            "a secret answer reached the export: {md}"
        );
        assert!(md.contains(komet_proto::WITHHELD_SECRET_ANSWER), "{md}");
    }

    #[test]
    fn concurrent_answers_converge_on_doc_order() {
        let a = SessionDoc::init("chat-1").unwrap();
        a.push_message(&question_entry("r1", komet_proto::QuestionSource::Native))
            .unwrap();
        let b = SessionDoc::from_doc({
            let d = LoroDoc::new();
            // The shared history already carries the open question's empty
            // `responses` container, so both devices append into the same list.
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });
        // Both devices answer offline.
        a.record_input_response("r1", &response("dev-a", "a", true))
            .unwrap();
        b.record_input_response("r1", &response("dev-b", "b", false))
            .unwrap();

        // Cross-import, the way the sync layer does it.
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .unwrap();
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .unwrap();
        b.doc().import(&a_update).unwrap();
        a.doc().import(&b_update).unwrap();

        let ra = recorded(&a, 0);
        let rb = recorded(&b, 0);
        assert_eq!(ra.len(), 2, "both answers survive the merge: {ra:?}");
        assert_eq!(ra, rb, "the two peers must elect the same winner");
        // Nothing stores "loser": the display winner is position 0, and the
        // other entry is rendered as the conflict because its index is > 0.
        let devices: Vec<&str> = ra.iter().map(|r| r.device.as_str()).collect();
        assert!(devices.contains(&"dev-a") && devices.contains(&"dev-b"));

        // A retried command replaying across the merged replicas restates the
        // answer, it does not mint a third row for the same device.
        a.record_input_response("r1", &response("dev-b", "b", false))
            .unwrap();
        b.record_input_response("r1", &response("dev-b", "b", false))
            .unwrap();
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .unwrap();
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .unwrap();
        b.doc().import(&a_update).unwrap();
        a.doc().import(&b_update).unwrap();
        let ra = recorded(&a, 0);
        assert_eq!(ra.len(), 2, "idempotent per device across peers: {ra:?}");
        assert_eq!(ra, recorded(&b, 0));
    }

    #[test]
    fn delivered_answer_is_marked() {
        // `applied` records what the host actually handed the agent, and stays
        // on that answer wherever doc order ends up putting it.
        let a = SessionDoc::init("chat-1").unwrap();
        a.push_message(&question_entry("r1", komet_proto::QuestionSource::Native))
            .unwrap();
        a.record_input_response("r1", &response("dev-a", "a", true))
            .unwrap();
        a.record_input_response("r1", &response("dev-b", "b", false))
            .unwrap();
        let responses = recorded(&a, 0);
        assert_eq!(responses.len(), 2);
        let applied: Vec<&str> = responses
            .iter()
            .filter(|r| r.applied)
            .map(|r| r.device.as_str())
            .collect();
        assert_eq!(
            applied,
            vec!["dev-a"],
            "exactly the delivered answer: {responses:?}"
        );
    }

    #[test]
    fn permission_decision_survives_the_round_trip() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&assistant_with(
            "m1",
            vec![MessagePart::Permission {
                id: "perm-r1".into(),
                request_id: "r1".into(),
                permission_kind: komet_proto::PermissionKind::Command {
                    cmdline: "git push".into(),
                },
                summary: "Run `git push`".into(),
                choices: vec![komet_proto::PermissionChoice::Allow],
                actions: vec![],
                resolved: false,
                decision: None,
                tool_call_id: Some("tool-9".into()),
                decided_by: None,
                decided_at: None,
                expires_at: None,
            }],
        ))
        .unwrap();

        assert!(
            doc.decide_permission(
                "r1",
                Some(&komet_proto::PermissionChoice::AllowAlways {
                    scope: komet_proto::Scope::Chat
                }),
                Some("dev-b"),
                Some(1_700_000_000_001),
            )
            .unwrap()
        );
        let entries = doc.read_entries().unwrap();
        let MessagePart::Permission {
            resolved,
            decision,
            decided_by,
            decided_at,
            tool_call_id,
            ..
        } = &entries[0].parts[0]
        else {
            panic!("permission part expected");
        };
        assert!(*resolved);
        assert_eq!(
            *decision,
            Some(komet_proto::PermissionChoice::AllowAlways {
                scope: komet_proto::Scope::Chat
            })
        );
        assert_eq!(decided_by.as_deref(), Some("dev-b"));
        assert_eq!(*decided_at, Some(1_700_000_000_001));
        assert_eq!(tool_call_id.as_deref(), Some("tool-9"));

        // Deny wins: once a Deny is recorded, a later Allow cannot rewrite it.
        doc.decide_permission("r1", Some(&komet_proto::PermissionChoice::Deny), None, None)
            .unwrap();
        doc.decide_permission(
            "r1",
            Some(&komet_proto::PermissionChoice::Allow),
            Some("dev-c"),
            Some(1_700_000_000_002),
        )
        .unwrap();
        let entries = doc.read_entries().unwrap();
        let MessagePart::Permission {
            decision,
            decided_by,
            ..
        } = &entries[0].parts[0]
        else {
            panic!("permission part expected");
        };
        assert_eq!(*decision, Some(komet_proto::PermissionChoice::Deny));
        assert_eq!(
            decided_by.as_deref(),
            Some("dev-b"),
            "the recorded decider is not overwritten by the attempt that lost"
        );
    }

    #[test]
    fn snapshot_round_trips_between_docs() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let bytes = doc.export_snapshot().unwrap();

        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        let restored = SessionDoc::from_doc(other);
        assert_eq!(
            restored.read_entries().unwrap(),
            doc.read_entries().unwrap()
        );
    }

    #[test]
    fn two_peers_converge_on_concurrent_inserts() {
        let a = SessionDoc::init("chat-1").unwrap();
        let b = SessionDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });
        a.push_message(&user_entry("m-a", "from a")).unwrap();
        b.push_message(&user_entry("m-b", "from b")).unwrap();

        // Cross-import updates.
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .unwrap();
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .unwrap();
        b.doc().import(&a_update).unwrap();
        a.doc().import(&b_update).unwrap();

        let ea = a.read_entries().unwrap();
        let eb = b.read_entries().unwrap();
        assert_eq!(ea, eb);
        assert_eq!(ea.len(), 2); // one insert from each peer, converged in the same order
    }

    #[test]
    fn segment_writer_streams_text_incrementally() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "Hel".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(&mut folded, &AgentEvent::TextDelta { text: "lo".into() });
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolResult {
                id: "tool-1".into(),
                is_error: false,
                output: None,
                diff: None,
            },
        );
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Some(MessageStatus::Complete));
        assert_eq!(entries[0].parts.len(), 2);
        match &entries[0].parts[0] {
            MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("unexpected {other:?}"),
        }
        match &entries[0].parts[1] {
            MessagePart::Tool {
                resolved, is_error, ..
            } => {
                assert!(*resolved);
                assert!(!*is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The ToolResult resolution path goes through `update_part_fields` —
    /// the stripped output summary, sidecar refs, and diff stats must survive
    /// the doc round trip (regression: output/diff were silently dropped
    /// there while `to_doc_part` carried them).
    #[test]
    fn segment_writer_round_trips_stripped_tool_fields() {
        let doc = SessionDoc::init("chat-2").unwrap();
        let mut writer = SegmentWriter::begin(&doc, "a1", "dev-a", 5).unwrap();

        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        writer.sync(&folded).unwrap();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: false,
                output: Some("total 0\nmore lines".into()),
                diff: Some(komet_proto::ToolDiff {
                    path: "/w/a.rs".into(),
                    old_text: Some("old\n".into()),
                    new_text: "new\n".into(),
                }),
            },
        );
        crate::parts::apply_sidecar_refs("chat-2", &mut folded);
        writer.sync(&folded).unwrap();
        writer.finish(&folded, MessageStatus::Complete).unwrap();

        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Tool {
                output,
                output_ref,
                output_bytes,
                diff,
                diff_ref,
                diff_stats,
                ..
            } => {
                // One-liner chips: the fold drops outputs entirely (journal
                // only), so even a direct apply_sidecar_refs call has no
                // output to key — diff stats still get their ref (this test
                // calls apply_sidecar_refs directly; the live fold no longer
                // does).
                assert_eq!(output.as_deref(), None);
                assert_eq!(output_ref.as_deref(), None);
                assert_eq!(*output_bytes, None);
                assert!(diff.is_none(), "no inline diff text in the doc");
                assert_eq!(diff_ref.as_deref(), Some("chat-2/t1.diff"));
                let stats = diff_stats.as_ref().expect("stats survive");
                assert_eq!(stats[0].path, "/w/a.rs");
                assert_eq!((stats[0].additions, stats[0].deletions), (1, 1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Old pre-strip docs carry inline `output`/`diff` — they must still read
    /// back (schema changes are serde-additive ONLY; old readers, old docs).
    #[test]
    fn pre_strip_doc_parts_still_round_trip() {
        let doc = SessionDoc::init("chat-3").unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
                output: Some("full inline output\nline 2".into()),
                diff: Some(komet_proto::ToolDiff {
                    path: "/w/a.rs".into(),
                    old_text: Some("old".into()),
                    new_text: "new".into(),
                }),
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
            created_at: 1,
            device_id: "dev-a".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        let entries = doc.read_entries().unwrap();
        match &entries[0].parts[0] {
            MessagePart::Tool { output, diff, .. } => {
                assert_eq!(output.as_deref(), Some("full inline output\nline 2"));
                assert_eq!(diff.as_ref().unwrap().new_text, "new");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn set_message_status_stamps_existing_entry() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut entry = user_entry("m1", "hello");
        entry.role = MessageRole::Assistant;
        entry.status = Some(MessageStatus::Streaming);
        doc.push_message(&entry).unwrap();

        assert!(
            doc.set_message_status("m1", MessageStatus::Aborted)
                .unwrap()
        );
        assert!(
            !doc.set_message_status("nope", MessageStatus::Aborted)
                .unwrap()
        );
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries[0].status, Some(MessageStatus::Aborted));
    }

    #[test]
    fn provider_switch_state_and_transcript_marker_round_trip() {
        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("m1", "hello")).unwrap();
        let marker = MessagePart::Switch {
            id: "switch-1".into(),
            from: "Claude Code".into(),
            to: "Devin".into(),
            from_harness: Some("claude-code".into()),
            to_harness: Some("devin".into()),
        };
        assert!(doc.append_switch_part("m1", &marker).unwrap());
        assert!(doc.append_switch_part("m1", &marker).unwrap());
        doc.mark_provider_switched().unwrap();
        doc.set_active_provider("devin").unwrap();
        let session = ProviderSessionMark {
            session_id: "devin-session".into(),
            cwd: "/workspace".into(),
            seen: Some("m1".into()),
        };
        doc.set_provider_session("devin", &session).unwrap();
        assert!(doc.provider_switched());
        assert_eq!(doc.active_provider().as_deref(), Some("devin"));
        assert_eq!(doc.provider_session("devin"), Some(session));
        let entries = doc.read_entries().unwrap();
        assert!(entries[0].parts.contains(&marker));
        assert_eq!(
            entries[0]
                .parts
                .iter()
                .filter(|part| matches!(part, MessagePart::Switch { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn command_queue_and_outcome_round_trip() {
        use crate::commands::{SessionCommandPayload, SessionCommandStatus};
        let doc = SessionDoc::init("chat-1").unwrap();
        let entry = SessionCommandEntry {
            id: "c1".into(),
            payload: SessionCommandPayload::Steer {
                prompt: "focus".into(),
                message_id: None,
            },
            issued_by: "dev-b".into(),
            issued_at: 10,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        doc.queue_command(&entry).unwrap();
        doc.set_command_status("c1", SessionCommandStatus::Applied, None)
            .unwrap();
        let commands = doc.read_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, SessionCommandStatus::Applied);
        assert_eq!(commands[0].payload, entry.payload);
    }

    #[test]
    fn tail_materializes_last_n_joined() {
        let doc = SessionDoc::init("chat-1").unwrap();
        for i in 0..5 {
            doc.push_message(&user_entry(&format!("m{i}"), &format!("msg {i}")))
                .unwrap();
        }
        let tail = materialize_tail(&doc, 99, 2).unwrap();
        assert_eq!(tail.total_messages, 5);
        assert_eq!(tail.messages.len(), 2);
        assert_eq!(tail.messages[1].id, "m4");
        assert_eq!(tail.chat_id, "chat-1");
    }

    /// 2026-08-10 incident: entries/parts missing strict fields must salvage
    /// field-by-field — a fresh reader importing a room's merged doc must
    /// never render a BLANK transcript because some writer (old app version,
    /// other-platform client, mangled export) omitted metadata.
    #[test]
    fn malformed_entries_salvage_instead_of_vanishing() {
        // Entry missing `id` + `deviceId`; one part missing `kind` but
        // carrying text; one part contentless (dropped).
        let v = serde_json::json!({
            "role": "assistant",
            "createdAt": 123,
            "parts": [
                { "id": "p1", "text": "still readable" },
                { "opaque": true },
                { "id": "p3", "kind": "text", "text": "well-formed" }
            ]
        });
        let entry = entry_from_json(v.clone()).expect("salvaged");
        assert!(
            entry.id.starts_with("recovered-"),
            "deterministic stand-in id"
        );
        let again = entry_from_json(v).expect("salvaged again");
        assert_eq!(entry.id, again.id, "recovered id is stable across reads");
        assert_eq!(entry.role, MessageRole::Assistant);
        assert_eq!(entry.created_at, 123);
        assert_eq!(
            entry.parts.len(),
            2,
            "text parts survive, contentless part dropped"
        );
        match &entry.parts[0] {
            MessagePart::Text { text, .. } => assert_eq!(text, "still readable"),
            other => panic!("unexpected {other:?}"),
        }

        // Tool part missing `kind` but with a parseable call salvages as Tool.
        let v = serde_json::json!({
            "role": "assistant",
            "createdAt": 1,
            "parts": [ { "id": "t1", "call": { "kind": "exec", "command": "ls" }, "output": "x" } ]
        });
        let entry = entry_from_json(v).expect("salvaged");
        assert!(matches!(
            &entry.parts[0],
            MessagePart::Tool { resolved: true, .. }
        ));

        // Only non-objects are truly unsalvageable.
        assert!(entry_from_json(serde_json::json!("garbage")).is_err());
        assert!(entry_from_json(serde_json::json!(42)).is_err());

        // Well-formed entries take the strict path unchanged.
        let v = serde_json::json!({
            "id": "m1", "role": "user", "createdAt": 5, "deviceId": "d",
            "parts": [ { "id": "p", "kind": "text", "text": "hi" } ]
        });
        let entry = entry_from_json(v).expect("strict");
        assert_eq!(entry.id, "m1");
    }

    #[test]
    fn test_truncate_messages_from() {
        let doc = SessionDoc::init("chat-truncate").unwrap();
        doc.push_message(&user_entry("m1", "first")).unwrap();
        doc.push_message(&user_entry("m2", "second")).unwrap();
        doc.push_message(&user_entry("m3", "third")).unwrap();
        assert_eq!(doc.read_entries().unwrap().len(), 3);

        // Truncate from index 1 (removes m2 and m3)
        doc.truncate_messages_from(1).unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "m1");

        // Push new message
        doc.push_message(&user_entry("m4", "new fourth")).unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].id, "m4");
    }

    fn compaction_part(
        compaction_id: &str,
        status: komet_proto::CompactionStatus,
        summary: Option<&str>,
    ) -> MessagePart {
        MessagePart::Compaction {
            id: format!("cmp-{compaction_id}"),
            compaction_id: compaction_id.into(),
            status,
            trigger: Some("auto".into()),
            tokens_before: Some(165_000),
            tokens_after: Some(12_000),
            error: None,
            summary: summary.map(str::to_owned),
            summary_bytes: summary.map(|s| s.len() as u64),
            summary_in_journal: summary.is_some(),
        }
    }

    #[test]
    fn compaction_part_round_trips_and_old_doc_ignores_it() {
        use komet_proto::CompactionStatus;

        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&assistant_with(
            "m1",
            vec![compaction_part(
                "c1",
                CompactionStatus::Completed,
                Some("recap"),
            )],
        ))
        .unwrap();
        let entries = doc.read_entries().unwrap();
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
            error,
        } = &entries[0].parts[0]
        else {
            panic!("compaction part expected, got {:?}", entries[0].parts[0]);
        };
        assert_eq!((id.as_str(), compaction_id.as_str()), ("cmp-c1", "c1"));
        assert_eq!(*status, CompactionStatus::Completed);
        assert_eq!((trigger.as_deref(), error.as_deref()), (Some("auto"), None));
        assert_eq!(
            (*tokens_before, *tokens_after),
            (Some(165_000), Some(12_000))
        );
        assert_eq!(
            (summary.as_deref(), *summary_bytes, *summary_in_journal),
            (Some("recap"), Some(5), true)
        );

        // An old doc row: a compaction written by a reader that predates the
        // columns must still land as a compaction, with the unknowns left
        // unknown rather than invented.
        let sparse = from_doc_part(DocPartJson {
            id: "cmp-c9".into(),
            kind: "compaction".into(),
            ..Default::default()
        });
        assert!(
            matches!(&sparse, MessagePart::Compaction {
                compaction_id, status, tokens_before, summary_bytes, summary_in_journal, ..
            } if compaction_id == "c9"
                && *status == CompactionStatus::InProgress
                && tokens_before.is_none()
                && summary_bytes.is_none()
                && !summary_in_journal),
            "sparse row degrades to \"a compaction happened\", not to nothing: {sparse:?}"
        );

        // The other direction: a reader that predates the kind must not lose
        // the entry — an unknown kind is an empty text part, and the strict
        // parse of the whole entry still succeeds (every column is optional).
        let mut raw = serde_json::to_value(&entries[0]).unwrap();
        raw["parts"][0]["kind"] = serde_json::json!("compaction-from-the-future");
        let read = entry_from_json(raw).expect("entry survives an unknown part kind");
        assert!(matches!(&read.parts[0], MessagePart::Text { text, .. } if text.is_empty()));
    }

    #[test]
    fn streaming_compaction_refines_one_part_instead_of_stacking() {
        // The streaming path (SegmentWriter::sync) re-writes the folded parts
        // every commit: an in-progress boundary must become ONE row that
        // settles, never a growing stack of the same cycle.
        use komet_proto::CompactionStatus;

        let doc = SessionDoc::init("chat-1").unwrap();
        let mut segment = SegmentWriter::begin(&doc, "m1", "dev-a", 1).unwrap();
        let mut folded = Vec::new();
        crate::parts::fold_event_into_parts(
            &mut folded,
            &AgentEvent::ContextCompaction {
                id: "c1".into(),
                status: CompactionStatus::InProgress,
                trigger: Some("auto".into()),
                tokens_before: Some(165_000),
                tokens_after: None,
                summary: None,
                summary_delta: None,
                error: None,
            },
        );
        segment.sync(&folded).unwrap();
        crate::parts::fold_event_into_parts(
            &mut folded,
            &AgentEvent::ContextCompaction {
                id: "c1".into(),
                status: CompactionStatus::Completed,
                trigger: None,
                tokens_before: None,
                tokens_after: Some(12_000),
                summary: None,
                summary_delta: Some("the recap".into()),
                error: None,
            },
        );
        segment.sync(&folded).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries[0].parts.len(), 1, "one boundary, refined in place");
        let MessagePart::Compaction {
            status,
            trigger,
            tokens_before,
            tokens_after,
            summary,
            summary_bytes,
            summary_in_journal,
            ..
        } = &entries[0].parts[0]
        else {
            panic!("compaction part expected");
        };
        assert_eq!(*status, CompactionStatus::Completed);
        assert_eq!(trigger.as_deref(), Some("auto"), "carried from frame one");
        assert_eq!(tokens_before, &Some(165_000));
        assert_eq!(tokens_after, &Some(12_000));
        assert_eq!(summary, &None, "the doc never got the text");
        assert_eq!(summary_bytes, &Some(9));
        assert!(*summary_in_journal);
    }

    fn compaction_event(
        id: &str,
        status: komet_proto::CompactionStatus,
        tokens_after: Option<u64>,
    ) -> AgentEvent {
        AgentEvent::ContextCompaction {
            id: id.into(),
            status,
            trigger: Some("auto".into()),
            tokens_before: Some(165_000),
            tokens_after,
            summary: None,
            summary_delta: None,
            error: None,
        }
    }

    #[test]
    fn inter_turn_compaction_creates_a_system_entry() {
        use komet_proto::CompactionStatus;

        let doc = SessionDoc::init("chat-1").unwrap();
        doc.push_message(&user_entry("u1", "first")).unwrap();

        // A boundary that arrives between two turns has no assistant entry to
        // fold into: it gets its own System entry, at the tail (doc order is
        // the transcript order).
        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &compaction_event("c1", CompactionStatus::InProgress, None),
        );
        doc.record_compaction_part(&folded[0], "dev-a", 7).unwrap();

        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 2, "the fact is appended, not merged");
        let last = entries.last().unwrap();
        assert_eq!(
            (last.role, last.status),
            (MessageRole::System, Some(MessageStatus::Complete))
        );
        assert_eq!(last.created_at, 7);
        assert_eq!(last.device_id, "dev-a");
        assert_eq!(last.parts.len(), 1);
        let MessagePart::Compaction {
            status,
            tokens_after,
            ..
        } = &last.parts[0]
        else {
            panic!("compaction part expected, got {:?}", last.parts[0]);
        };
        assert_eq!(*status, CompactionStatus::InProgress);
        assert_eq!(*tokens_after, None);

        // The same cycle settling refines that part instead of stacking a
        // second entry — one compaction, one part, wherever it lives.
        let mut same_cycle = Vec::new();
        fold_event_into_parts(
            &mut same_cycle,
            &compaction_event("c1", CompactionStatus::Completed, Some(12_000)),
        );
        doc.record_compaction_part(&same_cycle[0], "dev-a", 9)
            .unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 2, "the cycle keeps its one entry");
        let last = entries.last().unwrap();
        assert_eq!(last.id, "sys-cmp-c1");
        assert_eq!(
            last.parts[0],
            same_cycle[0].clone(),
            "refined in place with the whole cycle state"
        );

        // A second cycle is a second line: it must not reopen the first.
        let mut next_cycle = Vec::new();
        fold_event_into_parts(
            &mut next_cycle,
            &compaction_event("c2", CompactionStatus::InProgress, None),
        );
        doc.record_compaction_part(&next_cycle[0], "dev-a", 11)
            .unwrap();
        let entries = doc.read_entries().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].id, "sys-cmp-c1");
        assert_eq!(entries[2].id, "sys-cmp-c2");

        // `apply_compaction_tokens_after` fills only a gap: the count the agent
        // itself reported (or a sweep already wrote) is never overwritten.
        assert!(
            !doc.apply_compaction_tokens_after("cmp-c1", 99_000).unwrap(),
            "c1 already carries its own count"
        );
        assert!(doc.apply_compaction_tokens_after("cmp-c2", 99_000).unwrap());
        assert!(
            !doc.apply_compaction_tokens_after("cmp-c2", 1_000).unwrap(),
            "first writer wins"
        );
        assert!(
            !doc.apply_compaction_tokens_after("cmp-nope", 1).unwrap(),
            "unknown part: nothing stamped, no error"
        );
        let entries = doc.read_entries().unwrap();
        let MessagePart::Compaction { tokens_after, .. } = &entries[2].parts[0] else {
            unreachable!()
        };
        assert_eq!(*tokens_after, Some(99_000));
    }

    #[test]
    fn interrupted_compaction_is_failed_and_stays_failed() {
        use komet_proto::CompactionStatus;

        let doc = SessionDoc::init("chat-1").unwrap();
        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &compaction_event("c1", CompactionStatus::InProgress, None),
        );
        fold_event_into_parts(
            &mut folded,
            &compaction_event("c2", CompactionStatus::Completed, Some(12_000)),
        );
        doc.push_message(&assistant_with("m1", folded.clone()))
            .unwrap();
        // Another device's run owns its own fact: a boot sweep here must not
        // pronounce on a compaction it never saw start.
        let mut remote = Vec::new();
        fold_event_into_parts(
            &mut remote,
            &compaction_event("c3", CompactionStatus::InProgress, None),
        );
        doc.push_message(&SessionMessageEntry {
            id: "m2".into(),
            role: MessageRole::Assistant,
            parts: remote,
            created_at: 2,
            device_id: "dev-b".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();

        let note = "Run interrupted by backend restart";
        assert_eq!(doc.fail_interrupted_compactions("dev-a", note).unwrap(), 1);
        let entries = doc.read_entries().unwrap();
        let MessagePart::Compaction {
            status,
            error,
            tokens_before,
            ..
        } = &entries[0].parts[0]
        else {
            unreachable!()
        };
        assert_eq!(*status, CompactionStatus::Failed);
        assert_eq!(error.as_deref(), Some(note));
        assert_eq!(
            *tokens_before,
            Some(165_000),
            "the sweep keeps what happened"
        );
        // The settled cycle and the other device's part are untouched.
        let MessagePart::Compaction { status, .. } = &entries[0].parts[1] else {
            unreachable!()
        };
        assert_eq!(*status, CompactionStatus::Completed);
        let MessagePart::Compaction { status, error, .. } = &entries[1].parts[0] else {
            unreachable!()
        };
        assert_eq!(*status, CompactionStatus::InProgress);
        assert_eq!(*error, None);

        // Idempotent: a crash loop re-running the sweep finds nothing to stamp.
        assert_eq!(doc.fail_interrupted_compactions("dev-a", note).unwrap(), 0);
    }

    /// The spawn chip's terminal numbers are doc-resident (they survive a
    /// restart and reach every device), so they must survive the Loro round trip
    /// — and a doc written before they existed must still read back with them
    /// absent, not zeroed.
    #[test]
    fn subagent_terminal_fields_round_trip() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let mut folded = Vec::new();
        fold_event_into_parts(
            &mut folded,
            &AgentEvent::ToolCall {
                id: "spawn-a".into(),
                call: ToolCall::Unknown {
                    name: "Agent: investigate".into(),
                    input: None,
                },
            },
        );
        doc.push_message(&assistant_with("m1", folded.clone()))
            .unwrap();

        // Before the terminal write: the chip carries no numbers.
        let entries = doc.read_entries().unwrap();
        let MessagePart::Tool {
            subagent_duration_ms,
            subagent_tool_count,
            ..
        } = &entries[0].parts[0]
        else {
            unreachable!()
        };
        assert_eq!(*subagent_duration_ms, None);
        assert_eq!(*subagent_tool_count, None);

        // The terminal write re-pushes the refined part (the fold owns the
        // entry; a direct doc write would bypass the diffing writer).
        let mut settled = folded.clone();
        for part in &mut settled {
            if let MessagePart::Tool {
                id,
                subagent_status,
                subagent_duration_ms,
                subagent_tool_count,
                ..
            } = part
                && id == "spawn-a"
            {
                *subagent_status = Some(crate::parts::SubagentStatus::Done);
                *subagent_duration_ms = Some(41_000);
                *subagent_tool_count = Some(3);
            }
        }
        let doc2 = SessionDoc::init("chat-2").unwrap();
        doc2.push_message(&assistant_with("m1", settled)).unwrap();
        let entries = doc2.read_entries().unwrap();
        let MessagePart::Tool {
            subagent_status,
            subagent_duration_ms,
            subagent_tool_count,
            ..
        } = &entries[0].parts[0]
        else {
            unreachable!()
        };
        assert_eq!(*subagent_status, Some(crate::parts::SubagentStatus::Done));
        assert_eq!(*subagent_duration_ms, Some(41_000));
        assert_eq!(*subagent_tool_count, Some(3));
    }

    /// Recovery sweep: a chip still `running` when its run died is failed, and
    /// a crash loop never re-stamps it. The other device's live chip is left
    /// alone — this host cannot pronounce on a run it does not own.
    #[test]
    fn interrupted_subagent_is_failed_and_stays_failed() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let spawn = |id: &str| AgentEvent::ToolCall {
            id: id.into(),
            call: ToolCall::Unknown {
                name: "Agent: read".into(),
                input: None,
            },
        };
        let mut mine = Vec::new();
        fold_event_into_parts(&mut mine, &spawn("spawn-a"));
        // Tagged traffic is what proves the child was live.
        fold_event_into_parts(
            &mut mine,
            &AgentEvent::Subagent {
                parent_tool_use_id: "spawn-a".into(),
                event: Box::new(AgentEvent::TextDelta {
                    text: "working".into(),
                }),
            },
        );
        doc.push_message(&assistant_with("m1", mine.clone()))
            .unwrap();

        let mut other = Vec::new();
        fold_event_into_parts(&mut other, &spawn("spawn-b"));
        fold_event_into_parts(
            &mut other,
            &AgentEvent::Subagent {
                parent_tool_use_id: "spawn-b".into(),
                event: Box::new(AgentEvent::TextDelta {
                    text: "working".into(),
                }),
            },
        );
        doc.push_message(&SessionMessageEntry {
            id: "m2".into(),
            role: MessageRole::Assistant,
            parts: other,
            created_at: 2,
            device_id: "dev-b".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();

        assert_eq!(doc.fail_running_subagents("dev-a").unwrap(), 1);
        let entries = doc.read_entries().unwrap();
        let MessagePart::Tool {
            subagent_status,
            subagent_tool_count,
            subagent_duration_ms,
            ..
        } = &entries[0].parts[0]
        else {
            unreachable!()
        };
        assert_eq!(
            *subagent_status,
            Some(crate::parts::SubagentStatus::Failed),
            "a child that never settled reads as failed, not as still running"
        );
        assert_eq!(
            *subagent_duration_ms, None,
            "and the restart invents no duration: the journal has no timestamps"
        );
        assert_eq!(*subagent_tool_count, None);
        let MessagePart::Tool {
            subagent_status, ..
        } = &entries[1].parts[0]
        else {
            unreachable!()
        };
        assert_eq!(
            *subagent_status,
            Some(crate::parts::SubagentStatus::Running),
            "another device's live run keeps its own chip"
        );

        assert_eq!(doc.fail_running_subagents("dev-a").unwrap(), 0);
    }
}
