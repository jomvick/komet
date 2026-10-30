//! One subagent's transcript as a right-pane surface (transparency plan
//! Task 5).
//!
//! There is no child doc: the host's append-only run journal is the only copy of
//! what a subagent said, and `WatchSubagent` projects one feed out of it on
//! demand. This panel is a viewer of that projection — it keeps no content of
//! its own, writes nothing to the doc, and on a device that never ran the chat
//! says so out loud instead of showing a transcript that would read as "the
//! subagent said nothing".

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, Render, ScrollHandle, SharedString, Task, Window,
    div, prelude::*, px,
};

use komet_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus,
    fold_event_into_parts,
};
use komet_proto::AgentEvent;
use komet_rpc::methods;

use crate::markdown::parser::{BlockTree, IncrementalParser};
use crate::markdown::render::{self, RenderOptions};
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::{
    self, Row, RowKind, ToolItem, chip_header_row, compaction_line, detail_body, detail_section,
    error_chip, permission_row, question_row,
};

/// The band under the viewport bottom that still counts as "reading the end",
/// so a live feed keeps following until the user scrolls away (the transcript's
/// 70px pin, ported to a plain scroll container).
const FOLLOW_BAND: f32 = 70.0;

/// What a spawn chip says about its child, carried onto the tab when it is
/// clicked: the doc's chip is the authority for a settled cycle, `WatchActivity`
/// for a running one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSpawn {
    /// The spawn tool's part id — names the feed in the host's journal.
    pub parent_tool_use_id: SharedString,
    /// The chip's own label ("Agent: scan"): the tab title.
    pub title: SharedString,
    /// Model the spawn asked for, when it named one.
    pub model: Option<SharedString>,
    pub status: Option<SubagentStatus>,
    /// Terminal numbers from the chip's single doc write. `None` is "not
    /// measured on the host" — never rendered as a zero.
    pub duration_ms: Option<u64>,
    pub tool_count: Option<u64>,
}

/// One open subagent tab: which feed, on which chat and host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentOpen {
    pub chat_id: SharedString,
    /// The device whose journal backs the feed; `None` is this device.
    pub target_device_id: Option<SharedString>,
    pub spawn: SubagentSpawn,
}

/// What a live feed reported about itself, as the host measured it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentLive {
    pub status: SubagentStatus,
    pub tool_count: u64,
    /// The child's current tool, by label only ("Read", "Run") — its arguments
    /// never ride this frame.
    pub last_tool: Option<SharedString>,
    pub elapsed_ms: u64,
}

impl SubagentLive {
    /// The `WatchActivity` `subagentStatus` frame for `parent`, if this is one.
    /// Anything else (a tool-output or compaction frame) is `None` — a viewer
    /// that guessed at another kind's fields would invent numbers.
    pub fn from_frame(value: &serde_json::Value, parent: &str) -> Option<Self> {
        if value.get("kind")?.as_str()? != "subagentStatus" {
            return None;
        }
        if value.get("parentToolUseId")?.as_str()? != parent {
            return None;
        }
        let status = serde_json::from_value::<SubagentStatus>(value.get("status")?.clone()).ok()?;
        Some(Self {
            status,
            tool_count: value.get("toolCount")?.as_u64()?,
            last_tool: value
                .get("lastTool")
                .and_then(|v| v.as_str())
                .map(SharedString::from),
            elapsed_ms: value.get("elapsedMs")?.as_u64()?,
        })
    }
}

/// The spawn chip's status line: the lifecycle word, then the counters. The
/// doc's chip is the authority for the lifecycle — the live frame never
/// overrides it, because a frame is a moment that stops being refreshed while the
/// doc write is the run's last word: a chip left saying "running" by the last
/// frame before `Done` would say it forever. The doc has no lifecycle only for a
/// chat written before subagents were tracked, and there the live frame carries
/// it. Neither source backfills the other's numbers — a number that was never
/// measured stays unmentioned rather than reading as a zero. `None` means say
/// nothing: no lifecycle, no badge.
pub fn subagent_status_label(
    status: Option<SubagentStatus>,
    live: Option<&SubagentLive>,
    duration_ms: Option<u64>,
    tool_count: Option<u64>,
) -> Option<String> {
    let effective = status.or(live.map(|l| l.status))?;
    let word = match effective {
        SubagentStatus::Running => "agent running",
        SubagentStatus::Done => "agent done",
        SubagentStatus::Failed => "agent failed",
    };
    let mut segments = vec![word.to_string()];
    let running = effective == SubagentStatus::Running;
    let (count, elapsed_ms, last_tool) = match live.filter(|_| running) {
        Some(live) => (
            Some(live.tool_count),
            Some(live.elapsed_ms),
            live.last_tool.clone(),
        ),
        None => (tool_count, duration_ms, None),
    };
    if let Some(count) = count {
        segments.push(format!(
            "{count} {}",
            if count == 1 { "tool" } else { "tools" }
        ));
    }
    if let Some(ms) = elapsed_ms {
        segments.push(transcript::format_elapsed(ms_to_secs(ms)));
    }
    if let Some(tool) = last_tool {
        segments.push(tool.to_string());
    }
    Some(segments.join(" · "))
}

/// Millis → whole seconds, rounded, so a 900 ms cycle does not read "0s" and a
/// 41.6 s one does not read "42s" by truncation.
fn ms_to_secs(ms: u64) -> i64 {
    ((ms + 500) / 1000) as i64
}

/// The spawn chip's badge: the status line, tinted by the lifecycle it reports.
/// The caller hands it to [`crate::transcript::chip_header_row`] — and puts its
/// own click handler on it, because the badge is how a transcript opens a
/// subagent tab. `None` is "not a spawn chip": no badge, no affordance.
pub fn subagent_badge(
    status: Option<SubagentStatus>,
    live: Option<&SubagentLive>,
    duration_ms: Option<u64>,
    tool_count: Option<u64>,
    theme: &Theme,
) -> Option<gpui::Div> {
    let label = subagent_status_label(status, live, duration_ms, tool_count)?;
    let tint = match status.or(live.map(|l| l.status))? {
        SubagentStatus::Running => theme.busy,
        SubagentStatus::Done => theme.success,
        SubagentStatus::Failed => theme.danger,
    };
    Some(
        div()
            .flex_none()
            .h(px(18.0))
            .flex()
            .items_center()
            .text_size(px(11.0))
            .text_color(tint)
            .child(SharedString::from(label)),
    )
}

fn assistant_entry(
    parent_id: &str,
    segment: usize,
    parts: Vec<MessagePart>,
    status: MessageStatus,
) -> SessionMessageEntry {
    SessionMessageEntry {
        id: format!("{parent_id}#{segment}"),
        role: MessageRole::Assistant,
        parts,
        created_at: 0,
        device_id: String::new(),
        status: Some(status),
        continuation_of: None,
    }
}

/// Fold one feed's events into transcript entries. The same doc fold the parent
/// segment uses, so a child's text, tools and questions render exactly as they
/// would in a main transcript; a steer (`UserMessage`) opens a user entry, which
/// is how the parent's prompt shows up as what it is.
pub fn feed_entries(
    parent_id: &str,
    events: &[AgentEvent],
    settled: bool,
) -> Vec<SessionMessageEntry> {
    let mut entries: Vec<SessionMessageEntry> = Vec::new();
    let mut parts: Vec<MessagePart> = Vec::new();
    let mut segment = 0usize;

    for event in events {
        if let AgentEvent::UserMessage { text } = event {
            if !parts.is_empty() {
                entries.push(assistant_entry(
                    parent_id,
                    segment,
                    std::mem::take(&mut parts),
                    MessageStatus::Complete,
                ));
                segment += 1;
            }
            let id = format!("{parent_id}#u{segment}");
            entries.push(SessionMessageEntry {
                id: id.clone(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id,
                    text: text.clone(),
                }],
                created_at: 0,
                device_id: String::new(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            });
            segment += 1;
            continue;
        }
        fold_event_into_parts(&mut parts, event);
    }
    // The tail is the live part of the feed: streaming keeps the last blocks
    // split and fading instead of reflowing the whole reply per commit.
    if !parts.is_empty() {
        let tail_status = if settled {
            MessageStatus::Complete
        } else {
            MessageStatus::Streaming
        };
        entries.push(assistant_entry(
            parent_id,
            segment,
            std::mem::take(&mut parts),
            tail_status,
        ));
    }
    entries
}

/// Events the panel keeps from the stream: the run's `Done` is folded (it is the
/// child's own last word) but never re-rendered as a second error chip.
fn keep_event(event: &AgentEvent) -> bool {
    !matches!(event, AgentEvent::SessionStarted { .. })
}

/// One subagent tab.
pub struct SubagentPanel {
    state: Entity<AppState>,
    open: SubagentOpen,
    events: Vec<AgentEvent>,
    entries: Vec<SessionMessageEntry>,
    rows: Vec<Row>,
    /// Highest journal `seq` applied: replay and live overlap on it (the engine
    /// subscribes before it projects, so the same frame arrives twice).
    max_seq: Option<u64>,
    /// The child's own terminal `Done` (or the run's) arrived: the tail entry is
    /// no longer streaming.
    settled: bool,
    /// This host answered `unavailable` — the transcript is not empty, it is
    /// absent. The reason is shown verbatim; a guess would be worse.
    unavailable: Option<SharedString>,
    live: Option<SubagentLive>,
    started: bool,
    feed_task: Option<Task<()>>,
    activity_task: Option<Task<()>>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    /// Open chip details, keyed by tool part id (render-local, like the
    /// transcript's folds).
    open_chips: HashMap<SharedString, bool>,
    scroll: ScrollHandle,
}

/// What the panel asks the shell to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentEvent {
    /// A nested spawn inside this transcript: open another tab for it, one level
    /// deeper, over its own feed.
    OpenNested(Box<SubagentOpen>),
}

impl EventEmitter<SubagentEvent> for SubagentPanel {}

impl SubagentPanel {
    pub fn new(state: Entity<AppState>, open: SubagentOpen) -> Self {
        Self {
            state,
            open,
            events: Vec::new(),
            entries: Vec::new(),
            rows: Vec::new(),
            max_seq: None,
            settled: false,
            unavailable: None,
            live: None,
            started: false,
            feed_task: None,
            activity_task: None,
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            open_chips: HashMap::new(),
            scroll: ScrollHandle::default(),
        }
    }

    pub fn open(&self) -> &SubagentOpen {
        &self.open
    }

    /// The tab's title: what was spawned, not the word "subagent".
    pub fn tab_title(&self) -> SharedString {
        self.open.spawn.title.clone()
    }

    /// Idempotent, called from the render path like the diff surface: the
    /// subscriptions start when the surface first shows, not when the chat it
    /// belongs to merely gains a tab.
    pub fn ensure_started(&mut self, cx: &mut Context<Self>) {
        if self.started {
            return;
        }
        self.started = true;
        self.start_feed(cx);
        self.start_activity(cx);
    }

    fn start_feed(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut payload = serde_json::json!({
            "chatId": self.open.chat_id.as_ref(),
            "parentToolUseId": self.open.spawn.parent_tool_use_id.as_ref(),
        });
        if let Some(target) = self.open.target_device_id.as_ref()
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert(
                "targetDeviceId".to_string(),
                serde_json::Value::String(target.to_string()),
            );
        }
        self.feed_task = Some(cx.spawn(async move |this, cx| {
            let mut rx = match engine
                .client()
                .subscribe(methods::WATCH_SUBAGENT, payload)
                .await
            {
                Ok(rx) => rx,
                // An older engine has no `WatchSubagent`. Say so — the tab must
                // not open onto a blank transcript.
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.unavailable = Some(format!("the host did not answer ({err})").into());
                        cx.notify();
                    });
                    return;
                }
            };
            while let Some(value) = rx.recv().await {
                let applied = this.update(cx, |this, cx| {
                    if let Some(status) = value.get("status").and_then(|v| v.as_str()) {
                        if status == "unavailable" {
                            let reason = value
                                .get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("no run journal for this chat on this device");
                            this.unavailable = Some(reason.into());
                        }
                        // `{status:"ok", replayed:n}`: the header says the
                        // host has a journal. Nothing to apply.
                        cx.notify();
                        return;
                    }
                    let seq = value.get("seq").and_then(|v| v.as_u64());
                    if let Some(seq) = seq
                        && this.max_seq.is_some_and(|max| seq <= max)
                    {
                        // Replay/live overlap, deduped on the journal's own
                        // position — the frame is already in the transcript.
                        return;
                    }
                    let Some(event_value) = value.get("event") else {
                        return;
                    };
                    let Ok(event) = serde_json::from_value::<AgentEvent>(event_value.clone())
                    else {
                        return;
                    };
                    if let Some(seq) = seq {
                        this.max_seq = Some(seq);
                    }
                    if matches!(event, AgentEvent::Done { .. }) {
                        // The child's last word settles the header; the run's
                        // own Done (another `seq`, same stream) settles too.
                        this.settled = true;
                    }
                    if keep_event(&event) {
                        this.events.push(event);
                    }
                    this.rebuild(cx);
                });
                if applied.is_err() {
                    // The panel entity is gone — the tab closed mid-frame.
                    return;
                }
            }
            // The stream closed — the run is over. The tail stops streaming even
            // if no Done ever reached this feed.
            let _ = this.update(cx, |this, cx| {
                this.settled = true;
                this.rebuild(cx);
            });
        }));
    }

    /// The feed's live counters, from the host's own measurements. A second
    /// stream, because the transcript's `WatchActivity` watch belongs to the
    /// transcript entity and this tab outlives chat switches.
    fn start_activity(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut payload = serde_json::json!({ "chatId": self.open.chat_id.as_ref() });
        if let Some(target) = self.open.target_device_id.as_ref()
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert(
                "targetDeviceId".to_string(),
                serde_json::Value::String(target.to_string()),
            );
        }
        let parent = self.open.spawn.parent_tool_use_id.to_string();
        self.activity_task = Some(cx.spawn(async move |this, cx| {
            let mut rx = match engine
                .client()
                .subscribe(methods::WATCH_ACTIVITY, payload)
                .await
            {
                Ok(rx) => rx,
                // No live counters is not a broken tab: the header keeps the
                // doc's terminal numbers.
                Err(_) => return,
            };
            while let Some(value) = rx.recv().await {
                let Some(frame) = SubagentLive::from_frame(&value, &parent) else {
                    continue;
                };
                let alive = this
                    .update(cx, |this, cx| {
                        this.live = Some(frame);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
            }
        }));
    }

    /// Re-derive entries and rows from the event buffer, then follow the tail if
    /// the reader was already at the bottom.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.entries = feed_entries(
            self.open.spawn.parent_tool_use_id.as_ref(),
            &self.events,
            self.settled,
        );
        // Built from a taken copy: `rows_for` needs `&mut self` (the parse
        // caches live here) while the entry list is being walked.
        let entries = std::mem::take(&mut self.entries);
        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries.iter() {
            rows.extend(self.rows_for(entry));
        }
        self.entries = entries;
        self.rows = rows;
        let near_end = {
            let offset = self.scroll.offset().y;
            let max = self.scroll.max_offset().y;
            f32::from(max) - f32::from(offset) <= FOLLOW_BAND
        };
        if near_end {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    fn rows_for(&mut self, entry: &SessionMessageEntry) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let live_parsers = &mut self.live_parsers;
        let tree_cache = &mut self.tree_cache;
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            transcript::parse_for_row(streaming, key, text, live_parsers, tree_cache).0
        };
        transcript::rows_for_entry(entry, false, &mut parse)
    }

    /// The header line: what ran, on what model, and where the numbers came
    /// from. Nothing here is measured by the viewer — the doc's terminal write
    /// or the host's live frames are the only sources.
    fn render_header(&self, theme: &Theme) -> AnyElement {
        let spawn = &self.open.spawn;
        let status = subagent_status_label(
            spawn.status,
            self.live.as_ref(),
            spawn.duration_ms,
            spawn.tool_count,
        );
        // Tinted by the same lifecycle the word reports (doc first, live frame
        // only where the doc says nothing), so a header never reads "failed" in
        // the done green.
        let tint = match spawn.status.or(self.live.as_ref().map(|l| l.status)) {
            Some(SubagentStatus::Running) => theme.busy,
            Some(SubagentStatus::Done) => theme.success,
            Some(SubagentStatus::Failed) => theme.danger,
            None => theme.text_muted,
        };
        div()
            .flex_none()
            .w_full()
            .px(px(12.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .min_w_0()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(spawn.title.clone()),
                    )
                    .when_some(spawn.model.clone(), |row, model| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(model),
                        )
                    })
                    .when_some(status.map(SharedString::from), |row, status| {
                        row.child(
                            div()
                                .ml_auto()
                                .flex_none()
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tint)
                                .child(status),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(if self.unavailable.is_some() {
                        "projection of this host's run journal — unavailable here"
                    } else {
                        "read-only · projected from the host's run journal"
                    }),
            )
            .into_any_element()
    }

    fn render_unavailable(&self, theme: &Theme) -> AnyElement {
        let reason = self
            .unavailable
            .clone()
            .unwrap_or_else(|| "no run journal for this chat on this device".into());
        div()
            .flex_none()
            .m(px(12.0))
            .p(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.danger.opacity(0.24))
            .bg(theme.danger.opacity(0.06))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.danger)
                    .child("Transcript not available on this device"),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(reason),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(
                        "A subagent's transcript lives in the run journal of the device that ran it. \
                         Its status and final numbers stay visible here.",
                    ),
            )
            .into_any_element()
    }

    fn render_chip(
        &mut self,
        tool: &ToolItem,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let part_id: SharedString = tool.part_id.clone();
        let open = self.open_chips.get(&part_id).copied().unwrap_or(false);
        let key = format!("subagent-chip-{part_id}");
        // A nested spawn is another feed in the same journal: its badge opens a
        // second tab one level deeper instead of appending to this transcript.
        // The counters here are the doc's, not a live frame — this panel owns one
        // feed's activity watch, not its children's.
        let badge = tool.subagent_status.is_some().then(|| {
            let nested = SubagentOpen {
                chat_id: self.open.chat_id.clone(),
                target_device_id: self.open.target_device_id.clone(),
                spawn: SubagentSpawn {
                    parent_tool_use_id: part_id.clone(),
                    title: SharedString::from(tool.chip_label.to_string()),
                    model: tool.call.subagent_model().map(SharedString::from),
                    status: tool.subagent_status,
                    duration_ms: tool.subagent_duration_ms,
                    tool_count: tool.subagent_tool_count,
                },
            };
            subagent_badge(
                tool.subagent_status,
                None,
                tool.subagent_duration_ms,
                tool.subagent_tool_count,
                theme,
            )
            .expect("a chip with a status has a badge")
            .id(SharedString::from(format!("{key}-open")))
            .cursor_pointer()
            .on_click(cx.listener(move |_this: &mut Self, _, _, cx| {
                cx.stop_propagation();
                cx.emit(SubagentEvent::OpenNested(Box::new(nested.clone())));
            }))
        });
        let mut row = chip_header_row(tool, theme, open, badge)
            .id(SharedString::from(key.clone()))
            .cursor_pointer()
            .on_click(cx.listener(move |this: &mut Self, _, _, cx| {
                let entry = this.open_chips.entry(part_id.clone()).or_insert(false);
                *entry = !*entry;
                cx.notify();
            }));
        if open {
            if let Some(invocation) = tool.invocation.as_ref() {
                row = row.child(detail_section(
                    "Input",
                    detail_body(invocation, None, theme),
                    theme,
                ));
            }
            if let Some(detail) = tool.detail.as_ref() {
                row = row.child(detail_section(
                    if tool.is_error {
                        "Error output"
                    } else {
                        "Output"
                    },
                    detail_body(detail, None, theme),
                    theme,
                ));
            }
        }
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(row)
            .into_any_element()
    }

    fn render_row(
        &mut self,
        row: &Row,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &row.kind {
            RowKind::User { text, .. } => div()
                .w_full()
                .flex()
                .flex_col()
                .items_end()
                .py(px(4.0))
                .child(
                    div()
                        .max_w(px(520.0))
                        .rounded(px(10.0))
                        .bg(crate::theme::ink(0.08))
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_size(px(12.0))
                        .line_height(px(18.0))
                        .text_color(theme.text)
                        .child(text.clone()),
                )
                .into_any_element(),
            RowKind::Markdown { tree, block_ix } | RowKind::LiveMarkdown { tree, block_ix } => {
                let block_ix = *block_ix;
                let Some(top) = tree.blocks.get(block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let opts = RenderOptions::settled(row.id.clone());
                render::render_block(&top.block, block_ix, block_ix, &opts, theme, window, None)
            }
            RowKind::ToolGroup { tools, .. } => {
                let mut column = div().w_full().min_w_0().flex().flex_col().gap(px(0.0));
                for tool in tools.iter() {
                    column = column.child(self.render_chip(tool, theme, cx));
                }
                column.into_any_element()
            }
            RowKind::QuestionRow {
                header,
                question,
                source,
                status,
                responses,
                secret_question_ids,
            } => question_row(
                header.clone(),
                question.clone(),
                *source,
                *status,
                responses,
                secret_question_ids,
                theme,
            ),
            RowKind::PermissionRow {
                summary,
                resolved,
                decision,
                decided_by,
                decided_at,
                tool_call_id,
            } => permission_row(
                summary.clone(),
                *resolved,
                decision.as_ref(),
                decided_by.as_deref(),
                *decided_at,
                tool_call_id.as_deref(),
                theme,
            ),
            RowKind::ErrorChip { message, .. } => error_chip(message.clone(), theme),
            RowKind::Compaction {
                status,
                trigger,
                tokens_before,
                tokens_after,
                created_at,
                ..
            } => div()
                .w_full()
                .py(px(4.0))
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(compaction_line(
                    *status,
                    trigger.as_deref(),
                    *tokens_before,
                    *tokens_after,
                    *created_at,
                )))
                .into_any_element(),
        }
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_started(cx);
        let theme = Theme::of(cx).clone();
        let body: AnyElement = if self.rows.is_empty() {
            if self.unavailable.is_some() {
                self.render_unavailable(&theme)
            } else {
                div()
                    .flex_none()
                    .p(px(12.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_faint)
                    .child(if self.settled {
                        "The child ran no visible steps this cycle."
                    } else {
                        "Waiting for the feed's first frame…"
                    })
                    .into_any_element()
            }
        } else {
            let mut column = div().w_full().min_w_0().flex().flex_col().gap(px(4.0));
            if self.unavailable.is_some() {
                column = column.child(self.render_unavailable(&theme));
            }
            let rows = self.rows.clone();
            for row in rows {
                column = column.child(self.render_row(&row, window, &theme, cx));
            }
            column
                .child(
                    div()
                        .h(px(24.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(if self.settled {
                            SharedString::from("end of feed")
                        } else {
                            SharedString::from("streaming…")
                        }),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(self.render_header(&theme))
            .child(
                div()
                    .id("subagent-feed")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px(px(10.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(body),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(
        status: SubagentStatus,
        tool_count: u64,
        elapsed_ms: u64,
        last_tool: Option<&str>,
    ) -> SubagentLive {
        SubagentLive {
            status,
            tool_count,
            last_tool: last_tool.map(SharedString::from),
            elapsed_ms,
        }
    }

    /// While the child works, the numbers on the chip are the host's: its own
    /// counter, its own clock, the tool it is on right now.
    #[test]
    fn status_label_reports_the_hosts_live_counters() {
        let label = subagent_status_label(
            Some(SubagentStatus::Running),
            Some(&live(SubagentStatus::Running, 3, 12_400, Some("Read"))),
            None,
            None,
        )
        .expect("a running feed always says something");
        assert_eq!(label, "agent running · 3 tools · 12s · Read");
    }

    /// Once the child settled, the doc's single terminal write is the authority
    /// — and a live frame that arrived just before `Done` (still saying
    /// `running`, still mid-count) must not keep counting on a settled chip.
    #[test]
    fn status_label_switches_to_the_docs_terminal_write_when_settled() {
        let label = subagent_status_label(
            Some(SubagentStatus::Done),
            Some(&live(SubagentStatus::Running, 2, 9_000, Some("Run"))),
            Some(41_000),
            Some(3),
        )
        .expect("a settled chip with terminal numbers says them");
        assert_eq!(label, "agent done · 3 tools · 41s");
        // The other direction: a chat written before subagents were tracked has
        // no lifecycle on its chip, and there the live frame is what says the
        // child is working at all.
        let unlabeled = subagent_status_label(
            None,
            Some(&live(SubagentStatus::Running, 1, 1_200, Some("Search"))),
            None,
            None,
        )
        .expect("the live frame carries the lifecycle when the doc has none");
        assert_eq!(unlabeled, "agent running · 1 tool · 1s · Search");
    }

    /// A number the host never measured stays unmentioned. Rendering it as a
    /// zero would tell the user the child ran no tools, in a chat where it ran
    /// nine.
    #[test]
    fn status_label_leaves_an_unmeasured_number_out_instead_of_zeroing_it() {
        let label =
            subagent_status_label(Some(SubagentStatus::Running), None, None, None).expect("status");
        assert_eq!(label, "agent running");
        // No lifecycle at all (a chip from before this was tracked): say
        // nothing rather than guess.
        assert_eq!(
            subagent_status_label(None, None, Some(41_000), Some(3)),
            None
        );
    }

    #[test]
    fn status_label_declines_one_tool_in_the_singular() {
        let label =
            subagent_status_label(Some(SubagentStatus::Done), None, None, Some(1)).expect("count");
        assert_eq!(label, "agent done · 1 tool");
    }

    /// Rounded seconds: a 900 ms cycle must not read "0s" and a 41.6 s one must
    /// not read "41s" by truncation.
    #[test]
    fn status_label_rounds_seconds() {
        let label = subagent_status_label(
            Some(SubagentStatus::Running),
            Some(&live(SubagentStatus::Running, 1, 900, None)),
            None,
            None,
        )
        .expect("live");
        assert_eq!(label, "agent running · 1 tool · 1s");
    }

    /// The `subagentStatus` frame belongs to one feed. Reading another kind's
    /// fields (a tool's raw output, a compaction's sizes) as counters would
    /// invent numbers, and reading another feed's counters onto this chip would
    /// move a child's work onto its sibling.
    #[test]
    fn live_frames_are_gated_on_kind_and_parent() {
        let frame = serde_json::json!({
            "kind": "subagentStatus",
            "parentToolUseId": "tool-1",
            "status": "running",
            "toolCount": 2,
            "lastTool": "Read",
            "elapsedMs": 800,
        });
        assert!(SubagentLive::from_frame(&frame, "tool-1").is_some());
        assert!(SubagentLive::from_frame(&frame, "tool-2").is_none());
        let progress = serde_json::json!({"kind": "toolProgress", "id": "tool-1", "text": "$ c"});
        assert!(SubagentLive::from_frame(&progress, "tool-1").is_none());
        let compaction =
            serde_json::json!({"kind": "compaction", "toolCount": 2, "elapsedMs": 800});
        assert!(SubagentLive::from_frame(&compaction, "tool-1").is_none());
        // A frame missing a number is not a frame whose number is zero.
        let partial = serde_json::json!({
            "kind": "subagentStatus", "parentToolUseId": "tool-1", "status": "running",
        });
        assert!(SubagentLive::from_frame(&partial, "tool-1").is_none());
        // The tool label is optional — a feed that restarted has not started a
        // tool again yet.
        let no_tool = serde_json::json!({
            "kind": "subagentStatus", "parentToolUseId": "tool-1", "status": "running",
            "toolCount": 0, "elapsedMs": 12,
        });
        let live = SubagentLive::from_frame(&no_tool, "tool-1").expect("counters");
        assert_eq!(live.last_tool, None);
        assert_eq!(live.tool_count, 0);
    }

    /// A steer is the parent's own prompt inside the child's feed: it splits the
    /// reply into the segment before it and the segment after, so the transcript
    /// shows what was asked mid-run where it was asked.
    #[test]
    fn feed_entries_split_segments_at_a_steer() {
        let events = vec![
            AgentEvent::TextDelta {
                text: "first".into(),
            },
            AgentEvent::UserMessage {
                text: "deeper".into(),
            },
            AgentEvent::TextDelta {
                text: "second".into(),
            },
        ];
        let entries = feed_entries("tool-1", &events, true);
        let roles: Vec<_> = entries.iter().map(|e| e.role).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant
            ],
            "the steer reads as a user turn, not as part of the reply"
        );
        let ids: Vec<_> = entries.iter().map(|e| e.id.to_string()).collect();
        assert_eq!(
            ids,
            ["tool-1#0", "tool-1#u1", "tool-1#2"],
            "one counter across the feed, so no two entries can share a row key"
        );
        // Settled: every entry is complete, none keeps a streaming tail.
        assert!(
            entries
                .iter()
                .all(|e| e.status == Some(MessageStatus::Complete)),
            "a settled feed has no streaming tail: {entries:?}"
        );
    }

    /// Live, the tail entry streams (the transcript's incremental markdown,
    /// fade-in and all); the flushed segments before it are already complete.
    #[test]
    fn feed_entries_keep_only_the_tail_streaming() {
        let events = vec![
            AgentEvent::TextDelta {
                text: "before".into(),
            },
            AgentEvent::UserMessage {
                text: "again".into(),
            },
            AgentEvent::TextDelta {
                text: "after".into(),
            },
        ];
        let entries = feed_entries("tool-1", &events, false);
        assert_eq!(
            entries.iter().map(|e| e.status).collect::<Vec<_>>(),
            vec![
                Some(MessageStatus::Complete),
                Some(MessageStatus::Complete),
                Some(MessageStatus::Streaming)
            ]
        );
        let tail = entries.last().expect("tail");
        let MessagePart::Text { text, .. } = &tail.parts[0] else {
            panic!("expected a text part: {:?}", tail.parts);
        };
        assert_eq!(text, "after", "only what came after the steer");
    }

    /// The feed's own events, folded by the same doc fold the parent segment
    /// uses — a child's text is its child's text, not a summary of it.
    #[test]
    fn feed_entries_fold_the_childs_text_through_the_doc_fold() {
        let entries = feed_entries(
            "tool-9",
            &[
                AgentEvent::TextDelta {
                    text: "hello ".into(),
                },
                AgentEvent::TextDelta {
                    text: "world".into(),
                },
            ],
            true,
        );
        assert_eq!(entries.len(), 1, "one assistant entry for one segment");
        let MessagePart::Text { text, .. } = &entries[0].parts[0] else {
            panic!("expected a text part: {:?}", entries[0].parts);
        };
        assert_eq!(text, "hello world");
    }
}
