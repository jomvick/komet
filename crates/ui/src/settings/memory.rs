//! Settings → General → Memory: local project memory controls and inspection.
//!
//! The index is device-local and allow-listed — it never leaves this machine
//! and never stores prompts, commands, file contents or credentials. This
//! block is the visibility/control surface that makes the default-on choice
//! acceptable: where the data lives, how much there is, per-project switches
//! and exclusions, typed facts, graph/timeline inspection, explicit forget
//! actions, and one-way Obsidian export.

use gpui::{AnyElement, App, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use serde::Deserialize;

use komet_rpc::methods;

use crate::composer::ComposerInput;
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;
use crate::transcript::format_timestamp;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryStatus {
    pub available: bool,
    pub enabled: bool,
    /// Fixed raw-event retention (days) — reported for display only.
    pub retention_days: u32,
    pub dir: Option<String>,
    pub events: u64,
    pub chats: u64,
    pub bytes: u64,
    pub fts: bool,
    pub process_cpu_ms: Option<u64>,
    pub process_peak_rss_bytes: Option<u64>,
    pub untracked_chats: u64,
    /// Trailing 7-day instrumentation: briefings injected, memory tool calls,
    /// and the briefed/unbriefed `read_file` comparison.
    pub usage_week: Option<UsageWeek>,
    pub projects: Vec<MemoryProject>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageWeek {
    pub briefings: u64,
    pub briefing_chars: u64,
    pub memory_calls: Vec<MemoryCallMetric>,
    pub sessions_briefed: u64,
    pub sessions_unbriefed: u64,
    pub read_files_briefed: u64,
    pub read_files_unbriefed: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryCallMetric {
    pub metric: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryProject {
    pub project_id: String,
    pub root_path: Option<String>,
    pub enabled: bool,
    pub exclusions: Vec<String>,
    pub events: i64,
    pub last_activity_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryFact {
    pub id: i64,
    pub project_id: String,
    pub chat_id: Option<String>,
    pub kind: String,
    pub scope: String,
    pub content: String,
    pub source: String,
    pub source_agent: Option<String>,
    pub source_model: Option<String>,
    pub source_event_seq: Option<u64>,
    pub source_path: Option<String>,
    pub confidence: f64,
    pub user_confirmed: bool,
    pub valid_from_ms: i64,
    pub stale_at_ms: Option<i64>,
    pub replaced_by: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryGraphNeighbor {
    pub kind: String,
    pub key: String,
    pub label: String,
    pub relation: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryGraphFile {
    pub path: String,
    pub neighbors: Vec<MemoryGraphNeighbor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryActivity {
    pub chat_id: String,
    pub source_seq: u64,
    pub agent: String,
    pub kind: String,
    pub occurred_at_ms: i64,
    pub tool_name: Option<String>,
    pub relative_path: Option<String>,
    pub outcome: Option<String>,
}

/// Which destructive action is waiting for its second click.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    ForgetProject(String),
    ForgetAll,
    ForgetFact(i64),
    PromoteFact(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FactFilter {
    #[default]
    Current,
    AgentAssertions,
    UserConfirmed,
    LowConfidence,
    Stale,
}

pub struct MemoryPage {
    state: Entity<AppState>,
    status: Loadable<MemoryStatus>,
    loading: bool,
    task: Option<Task<()>>,
    /// Facts/graph/activity fetch for the expanded project — kept separate
    /// from `task` so a detail reload never cancels an in-flight status load.
    detail_task: Option<Task<()>>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// The engine the displayed status belongs to — memory is device-local,
    /// so a workspace/device switch must reload, never show the old index.
    engine: Option<EngineHandle>,
    /// Bumped on every load AND every engine rebind: async results carrying
    /// an older generation are dropped, so a slow reply from the previous
    /// engine can't overwrite the new one's status.
    generation: u64,
    /// Obsidian export destination draft (absolute folder path).
    export_input: Entity<ComposerInput>,
    /// Project whose detail row is expanded, plus its exclusions draft.
    expanded: Option<String>,
    exclusions_input: Option<Entity<ComposerInput>>,
    facts: Vec<MemoryFact>,
    graph_files: Vec<MemoryGraphFile>,
    activity: Vec<MemoryActivity>,
    facts_loading: bool,
    facts_generation: u64,
    fact_filter: FactFilter,
    /// Whether the per-project list is unfolded.
    projects_open: bool,
    confirm: Option<Confirm>,
}

impl MemoryPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let export_input = cx.new(|cx| ComposerInput::new("Export folder…", cx));
        cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx))
            .detach();
        let mut page = Self {
            state,
            status: Loadable::Idle,
            loading: false,
            task: None,
            detail_task: None,
            error: None,
            notice: None,
            engine: None,
            generation: 0,
            export_input,
            expanded: None,
            exclusions_input: None,
            facts: Vec::new(),
            graph_files: Vec::new(),
            activity: Vec::new(),
            facts_loading: false,
            facts_generation: 0,
            fact_filter: FactFilter::Current,
            projects_open: true,
            confirm: None,
        };
        page.load(cx);
        page
    }

    /// Reload when the displayed engine changes (local ↔ paired device, or a
    /// reconnect that swaps the backend). State notifies on every change, so
    /// the engine identity — not the notification — is the trigger.
    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let current = self.state.read(cx).engine().cloned();
        let same = match (&current, &self.engine) {
            (Some(a), Some(b)) => a.same_engine(b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.engine = current;
        self.generation += 1;
        self.status = Loadable::Idle;
        self.expanded = None;
        self.exclusions_input = None;
        self.facts.clear();
        self.graph_files.clear();
        self.activity.clear();
        self.facts_loading = false;
        self.facts_generation += 1;
        self.fact_filter = FactFilter::Current;
        self.projects_open = true;
        self.confirm = None;
        self.load(cx);
    }

    /// Params with the `targetDeviceId` passthrough merged in — inside an
    /// environment connection the memory RPCs must name the device that owns
    /// the store (resolved per call like ProvidersPage).
    fn with_target(&self, value: serde_json::Value, cx: &App) -> serde_json::Value {
        let mut value = value;
        let target = self.state.read(cx).harness_target_device();
        if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if !matches!(self.status, Loadable::Ready(_)) {
                self.status = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        self.engine = Some(engine.clone());
        self.generation += 1;
        let generation = self.generation;
        if matches!(self.status, Loadable::Idle) {
            self.status = Loadable::Loading;
        }
        self.loading = true;
        let params = self.with_target(serde_json::json!({}), cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_MEMORY_STATUS, params)
                .await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return; // a newer load (or engine switch) superseded this
                }
                page.loading = false;
                page.apply(result, cx);
                if let Some(project_id) = page.expanded.clone() {
                    page.load_facts(&project_id, cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply(
        &mut self,
        result: Result<serde_json::Value, komet_rpc::RpcError>,
        _cx: &mut Context<Self>,
    ) {
        self.status = match result.map_err(|err| err.to_string()).and_then(|value| {
            serde_json::from_value::<MemoryStatus>(value).map_err(|e| e.to_string())
        }) {
            Ok(status) => Loadable::Ready(status),
            Err(message) => Loadable::Error(message),
        };
    }

    /// Fire a settings RPC, then reload the status on success.
    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(params, cx);
        let generation = self.generation;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return; // engine switched mid-call: the new page reloads itself
                }
                match result {
                    Ok(_) => {
                        page.error = None;
                        page.confirm = None;
                        page.load(cx);
                    }
                    Err(err) => {
                        page.error = Some(SharedString::from(err.to_string()));
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn toggle_global(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.status.ready() else {
            return;
        };
        self.call(
            methods::SET_MEMORY_ENABLED,
            serde_json::json!({ "enabled": !status.enabled }),
            cx,
        );
    }

    fn toggle_project(&mut self, project_id: String, enabled: bool, cx: &mut Context<Self>) {
        self.call(
            methods::SET_MEMORY_PROJECT_ENABLED,
            serde_json::json!({ "projectId": project_id, "enabled": enabled }),
            cx,
        );
    }

    fn toggle_expanded(&mut self, project: &MemoryProject, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(project.project_id.as_str()) {
            self.expanded = None;
            self.exclusions_input = None;
            self.facts.clear();
            self.graph_files.clear();
            self.activity.clear();
            self.facts_generation += 1;
        } else {
            self.expanded = Some(project.project_id.clone());
            let input = cx.new(|cx| ComposerInput::new("Path exclusions (one per line)", cx));
            input.update(cx, |input, cx| {
                input.set_text(project.exclusions.join("\n"), cx)
            });
            self.exclusions_input = Some(input);
            self.load_facts(&project.project_id, cx);
        }
        cx.notify();
    }

    fn toggle_projects_open(&mut self, cx: &mut Context<Self>) {
        self.projects_open = !self.projects_open;
        cx.notify();
    }

    /// Facts + code graph + activity timeline for the expanded project, in
    /// one task. Results are dropped when the expansion moved on.
    fn load_facts(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.facts_generation += 1;
        let facts_generation = self.facts_generation;
        let page_generation = self.generation;
        let project_id = project_id.to_string();
        let include_stale = self.fact_filter == FactFilter::Stale;
        let params = self.with_target(
            serde_json::json!({
                "projectId": project_id,
                "includeStale": include_stale,
                "limit": 200
            }),
            cx,
        );
        let graph_params = self.with_target(
            serde_json::json!({"projectId": project_id, "limit": 25}),
            cx,
        );
        let activity_params = self.with_target(
            serde_json::json!({"projectId": project_id, "limit": 50}),
            cx,
        );
        self.facts_loading = true;
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_MEMORY_FACTS, params)
                .await;
            let graph_result = engine
                .client()
                .call(methods::GET_MEMORY_PROJECT_GRAPH, graph_params)
                .await;
            let activity_result = engine
                .client()
                .call(methods::GET_MEMORY_ACTIVITY, activity_params)
                .await;
            this.update(cx, |page, cx| {
                if page.generation != page_generation
                    || page.facts_generation != facts_generation
                    || page.expanded.as_deref() != Some(project_id.as_str())
                {
                    return;
                }
                page.facts_loading = false;
                match result
                    .map_err(|error| error.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<serde_json::Value>(value)
                            .map_err(|error| error.to_string())
                    })
                    .and_then(|value| {
                        serde_json::from_value::<Vec<MemoryFact>>(
                            value.get("facts").cloned().unwrap_or_default(),
                        )
                        .map_err(|error| error.to_string())
                    }) {
                    Ok(facts) => page.facts = facts,
                    Err(error) => page.error = Some(SharedString::from(error)),
                }
                match graph_result
                    .map_err(|error| error.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<serde_json::Value>(value)
                            .map_err(|error| error.to_string())
                    })
                    .and_then(|value| {
                        serde_json::from_value::<Vec<MemoryGraphFile>>(
                            value.get("files").cloned().unwrap_or_default(),
                        )
                        .map_err(|error| error.to_string())
                    }) {
                    Ok(files) => page.graph_files = files,
                    Err(error) => page.error = Some(SharedString::from(error)),
                }
                match activity_result
                    .map_err(|error| error.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<serde_json::Value>(value)
                            .map_err(|error| error.to_string())
                    })
                    .and_then(|value| {
                        serde_json::from_value::<Vec<MemoryActivity>>(
                            value.get("events").cloned().unwrap_or_default(),
                        )
                        .map_err(|error| error.to_string())
                    }) {
                    Ok(activity) => page.activity = activity,
                    Err(error) => page.error = Some(SharedString::from(error)),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn cycle_fact_filter(&mut self, project_id: &str, cx: &mut Context<Self>) {
        self.fact_filter = match self.fact_filter {
            FactFilter::Current => FactFilter::AgentAssertions,
            FactFilter::AgentAssertions => FactFilter::UserConfirmed,
            FactFilter::UserConfirmed => FactFilter::LowConfidence,
            FactFilter::LowConfidence => FactFilter::Stale,
            FactFilter::Stale => FactFilter::Current,
        };
        self.load_facts(project_id, cx);
        cx.notify();
    }

    fn ask_forget_fact(&mut self, id: i64, cx: &mut Context<Self>) {
        self.confirm = Some(Confirm::ForgetFact(id));
        cx.notify();
    }

    fn ask_promote_fact(&mut self, id: i64, cx: &mut Context<Self>) {
        self.confirm = Some(Confirm::PromoteFact(id));
        cx.notify();
    }

    fn save_exclusions(&mut self, project_id: String, cx: &mut Context<Self>) {
        let Some(input) = &self.exclusions_input else {
            return;
        };
        let exclusions: Vec<String> = input
            .read(cx)
            .text()
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        self.call(
            methods::SET_MEMORY_PROJECT_EXCLUSIONS,
            serde_json::json!({ "projectId": project_id, "exclusions": exclusions }),
            cx,
        );
    }

    fn export_obsidian(&mut self, project_id: String, cx: &mut Context<Self>) {
        let destination = self.export_input.read(cx).text().trim().to_string();
        if destination.is_empty() || !std::path::Path::new(&destination).is_absolute() {
            self.error = Some(SharedString::from(
                "Choose an absolute local folder for the Obsidian export.",
            ));
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some(SharedString::from("Engine not connected."));
            cx.notify();
            return;
        };
        let page_generation = self.generation;
        let params = self.with_target(
            serde_json::json!({
                "projectId": project_id,
                "destination": destination,
            }),
            cx,
        );
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::EXPORT_MEMORY_OBSIDIAN, params)
                .await;
            this.update(cx, |page, cx| {
                if page.generation != page_generation {
                    return;
                }
                match result {
                    Ok(value) => {
                        let facts = value["facts"].as_u64().unwrap_or(0);
                        let sessions = value["sessions"].as_u64().unwrap_or(0);
                        let path = value["destination"].as_str().unwrap_or("the selected folder");
                        page.error = None;
                        page.notice = Some(SharedString::from(format!(
                            "Exported {facts} fact(s) and {sessions} session(s) to {path}. SQLite remains canonical; export is one-way."
                        )));
                    }
                    Err(error) => {
                        page.error = Some(SharedString::from(error.to_string()));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn ask_forget_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.confirm = Some(Confirm::ForgetProject(project_id));
        cx.notify();
    }

    fn ask_forget_all(&mut self, cx: &mut Context<Self>) {
        self.confirm = Some(Confirm::ForgetAll);
        cx.notify();
    }

    fn cancel_confirm(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    fn confirm_forget(&mut self, cx: &mut Context<Self>) {
        match self.confirm.clone() {
            Some(Confirm::ForgetProject(project_id)) => {
                self.call(
                    methods::FORGET_MEMORY_PROJECT,
                    serde_json::json!({ "projectId": project_id }),
                    cx,
                );
            }
            Some(Confirm::ForgetAll) => {
                self.call(methods::FORGET_ALL_MEMORY, serde_json::json!({}), cx);
            }
            Some(Confirm::ForgetFact(id)) => {
                self.call(
                    methods::FORGET_MEMORY_FACT,
                    serde_json::json!({ "id": id }),
                    cx,
                );
            }
            Some(Confirm::PromoteFact(id)) => {
                self.call(
                    methods::PROMOTE_MEMORY_FACT,
                    serde_json::json!({ "id": id, "confirmed": true }),
                    cx,
                );
            }
            None => {}
        }
    }

    fn import_history(&mut self, cx: &mut Context<Self>) {
        self.call(methods::IMPORT_MEMORY_HISTORY, serde_json::json!({}), cx);
        self.notice = Some(SharedString::from(
            "Indexing existing journals in the background — new briefings pick it up as it lands.",
        ));
        cx.notify();
    }

    fn open_folder(&self, path: &str) {
        reveal_path(path);
    }
}

/// Best-effort "show this folder in the OS file manager". The UI spawns the
/// platform opener directly (same approach as the notification helpers).
fn reveal_path(path: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(err) = result {
        tracing::warn!(error = %err, path, "memory dir open failed");
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_cpu_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{:.1} min", ms as f64 / 60_000.0)
    } else if ms >= 1_000 {
        format!("{:.1} s", ms as f64 / 1_000.0)
    } else {
        format!("{ms} ms")
    }
}

fn project_label(project: &MemoryProject) -> String {
    project
        .root_path
        .as_deref()
        .and_then(|root| {
            std::path::Path::new(root)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            // git-common-dir ids are `git:<sha256>` — show a stable short form.
            // `get` keeps non-ASCII hand-edited ids from panicking on a slice.
            let id = &project.project_id;
            let short = id.len().min(12);
            format!("{}…", id.get(..short).unwrap_or(id))
        })
}

/// The two instrumentation lines under Storage: what the week looked like,
/// then the briefed/unbriefed `read_file` comparison the counters exist for.
fn usage_lines(usage: &UsageWeek) -> Vec<String> {
    let calls: u64 = usage.memory_calls.iter().map(|call| call.count).sum();
    let mut lines = vec![format!(
        "Last 7 days: {} briefing(s) injected · {} chars · {} memory tool call(s)",
        usage.briefings, usage.briefing_chars, calls
    )];
    if usage.sessions_briefed + usage.sessions_unbriefed > 0 {
        let per_session = |reads: u64, sessions: u64| {
            if sessions == 0 {
                "—".to_string()
            } else {
                format!("{:.1}", reads as f64 / sessions as f64)
            }
        };
        lines.push(format!(
            "read_file per session — briefed {} ({} sessions) · unbriefed {} ({} sessions)",
            per_session(usage.read_files_briefed, usage.sessions_briefed),
            usage.sessions_briefed,
            per_session(usage.read_files_unbriefed, usage.sessions_unbriefed),
            usage.sessions_unbriefed,
        ));
    }
    lines
}

impl MemoryPage {
    /// Human name for a project id — the raw `git:<sha256>` id is unreadable
    /// in a confirmation prompt.
    fn project_display_name(&self, project_id: &str) -> String {
        self.status
            .ready()
            .and_then(|status| {
                status
                    .projects
                    .iter()
                    .find(|project| project.project_id == project_id)
            })
            .map(project_label)
            .unwrap_or_else(|| project_id.to_string())
    }

    fn confirm_strip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.confirm.clone()?;
        let (message, danger_id, action_label) = match &confirm {
            Confirm::ForgetProject(project_id) => (
                SharedString::from(format!(
                    "Forget every retained activity record for {}? This cannot be undone.",
                    self.project_display_name(project_id)
                )),
                "memory-forget-project-confirm",
                "Forget",
            ),
            Confirm::ForgetAll => (
                SharedString::from(
                    "Forget ALL retained agent memory on this device? This cannot be undone.",
                ),
                "memory-forget-all-confirm",
                "Forget",
            ),
            Confirm::ForgetFact(id) => (
                SharedString::from(format!("Forget memory fact {id}? This cannot be undone.")),
                "memory-forget-fact-confirm",
                "Forget",
            ),
            Confirm::PromoteFact(id) => (
                SharedString::from(format!(
                    "Promote fact {id} to device-global memory? Every project on this device may use it."
                )),
                "memory-promote-fact-confirm",
                "Promote",
            ),
        };
        Some(
            widgets::warning_strip(theme, message)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .pt(px(6.0))
                        .child(
                            widgets::ghost_action(theme)
                                .id("memory-confirm-cancel")
                                .text_size(px(12.0))
                                .hover(|s| widgets::ghost_hover(theme, s))
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_confirm(cx)))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id(danger_id)
                                .text_size(px(12.0))
                                .text_color(theme.danger)
                                .hover(|s| s.bg(crate::theme::ink(0.06)))
                                .on_click(cx.listener(|this, _, _, cx| this.confirm_forget(cx)))
                                .child(SharedString::from(action_label)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn storage_card(
        &self,
        status: &MemoryStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dir = status.dir.clone().unwrap_or_default();
        let mut card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .child(widgets::row_tile(theme, crate::icons::HARD_DRIVE))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(theme, "Storage"))
                        .child(widgets::meta_line(
                            theme,
                            vec![
                                SharedString::from(format!(
                                    "{} · {} events · {} chats",
                                    format_bytes(status.bytes),
                                    status.events,
                                    status.chats
                                ))
                                .into_any_element(),
                                SharedString::from(if status.fts {
                                    "full-text search"
                                } else {
                                    "LIKE search"
                                })
                                .into_any_element(),
                            ],
                        )),
                )
                .when(!dir.is_empty(), |el| {
                    el.child(
                        widgets::ghost_action(theme)
                            .id("memory-open-folder")
                            .flex_none()
                            .text_size(px(12.0))
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let _ = cx;
                                this.open_folder(&dir);
                            }))
                            .child(SharedString::from("Open folder")),
                    )
                }),
        );
        if let Some(dir) = &status.dir {
            card = card.child(
                div()
                    .px(px(20.0))
                    .pb(px(10.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(dir.clone())),
            );
        }
        if let Some(usage) = &status.usage_week {
            for line in usage_lines(usage) {
                card = card.child(widgets::card_row(theme, false).child(widgets::meta_line(
                    theme,
                    vec![SharedString::from(line).into_any_element()],
                )));
            }
        }
        if let (Some(cpu_ms), Some(rss_bytes)) =
            (status.process_cpu_ms, status.process_peak_rss_bytes)
        {
            card = card.child(widgets::card_row(theme, false).child(widgets::meta_line(
                theme,
                vec![SharedString::from(format!(
                    "Engine lifetime: {} CPU · {} peak RAM (engine-wide, not only memory).",
                    format_cpu_duration(cpu_ms),
                    format_bytes(rss_bytes)
                ))
                .into_any_element()],
            )));
        }
        card.into_any_element()
    }

    fn control_card(
        &self,
        status: &MemoryStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = status.enabled;
        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, crate::icons::CHECKLIST))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Agent memory"))
                            .child(widgets::meta_line(
                                theme,
                                vec![SharedString::from(
                                    "Local activity index shared by agents on this device. On by default; turning it off stops recording and activity from the off period is never indexed later. Existing data is kept.",
                                )
                                .into_any_element()],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(theme, enabled)
                            .id("memory-global-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_global(cx))),
                    ),
            )
            .child(widgets::card_row(theme, false).child(widgets::meta_line(
                theme,
                vec![SharedString::from(format!(
                    "Raw activity is kept for {} days, then pruned.",
                    status.retention_days
                ))
                .into_any_element()],
            )))
            .into_any_element()
    }

    fn projects_card(
        &self,
        status: &MemoryStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = status.projects.len();
        let mut card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .cursor_pointer()
                .id("memory-projects-toggle")
                .on_click(cx.listener(|this, _, _, cx| this.toggle_projects_open(cx)))
                .child(widgets::row_tile(theme, crate::icons::FOLDER))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(theme, "Projects"))
                        .child(widgets::meta_line(
                            theme,
                            vec![SharedString::from(if count == 0 {
                                "No projects indexed yet — memory appears here after the first agent session.".to_string()
                            } else {
                                format!("{count} project(s) — expand a row for facts, graph, timeline and export.")
                            })
                            .into_any_element()],
                        )),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(if self.projects_open {
                            "Hide"
                        } else {
                            "Show"
                        })),
                ),
        );
        if self.projects_open {
            for (index, project) in status.projects.iter().enumerate() {
                card = card.child(self.project_row(project, index == 0, theme, cx));
                if self.expanded.as_deref() == Some(project.project_id.as_str()) {
                    card = card.child(self.project_detail(project, theme, cx));
                }
            }
        }
        card.into_any_element()
    }

    fn project_row(
        &self,
        project: &MemoryProject,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expand_project = project.clone();
        let toggle_project_id = project.project_id.clone();
        let enabled = project.enabled;
        let expanded = self.expanded.as_deref() == Some(project.project_id.as_str());
        let label = project_label(project);
        let row_id = SharedString::from(format!("memory-project-{}", project.project_id));
        let toggle_id = SharedString::from(format!("memory-project-toggle-{}", project.project_id));
        let events_label = format!("{} events", project.events);
        widgets::card_row(theme, first)
            .cursor_pointer()
            .id(row_id)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(&expand_project, cx)))
            .child(widgets::row_tile(theme, crate::icons::FOLDER))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(widgets::row_title(theme, label))
                            .when(!enabled, |el| el.child(widgets::badge(theme, "Off"))),
                    )
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            SharedString::from(events_label).into_any_element(),
                            SharedString::from(if expanded { "expanded" } else { " " })
                                .into_any_element(),
                        ],
                    )),
            )
            .child(
                widgets::toggle_switch(theme, enabled)
                    .id(toggle_id)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_project(toggle_project_id.clone(), !enabled, cx)
                    })),
            )
            .into_any_element()
    }

    fn project_detail(
        &self,
        project: &MemoryProject,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let export_project_id = project.project_id.clone();
        let exclusions_project_id = project.project_id.clone();
        let forget_project_id = project.project_id.clone();
        let mut detail = div()
            .px(px(20.0))
            .pb(px(14.0))
            .pl(px(56.0))
            .flex()
            .flex_col()
            .gap(px(10.0));

        if let Some(root) = &project.root_path {
            detail = detail.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(root.clone())),
            );
        }
        detail = detail.child(self.facts_detail(project, theme, cx));
        detail = detail
            .child(self.graph_detail(theme))
            .child(self.activity_detail(theme));
        detail = detail
            .child(widgets::field_label(theme, "Obsidian export"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0().child(self.export_input.clone()))
                    .child(
                        widgets::ghost_action(theme)
                            .id("memory-obsidian-export")
                            .flex_none()
                            .text_size(px(12.0))
                            .hover(|style| widgets::ghost_hover(theme, style))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.export_obsidian(export_project_id.clone(), cx)
                            }))
                            .child(SharedString::from("Export")),
                    ),
            )
            .child(widgets::meta_line(
                theme,
                vec![SharedString::from(
                    "One-way Markdown with YAML frontmatter and wikilinks. Choose a new folder; existing folders are only updated when already owned by Komet.",
                )
                .into_any_element()],
            ));
        detail = detail.child(widgets::field_label(theme, "Path exclusions"));
        if let Some(input) = &self.exclusions_input {
            detail = detail.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0().child(input.clone()))
                    .child(
                        widgets::ghost_action(theme)
                            .id("memory-exclusions-save")
                            .flex_none()
                            .text_size(px(12.0))
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.save_exclusions(exclusions_project_id.clone(), cx)
                            }))
                            .child(SharedString::from("Save")),
                    ),
            );
        }
        detail = detail.child(
            div().flex().flex_row().child(
                widgets::ghost_action(theme)
                    .id("memory-forget-project")
                    .text_size(px(12.0))
                    .text_color(theme.danger)
                    .hover(|s| s.bg(crate::theme::ink(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.ask_forget_project(forget_project_id.clone(), cx)
                    }))
                    .child(SharedString::from("Forget this project")),
            ),
        );
        detail.into_any_element()
    }

    fn facts_detail(
        &self,
        project: &MemoryProject,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_id = project.project_id.clone();
        let filter_label = match self.fact_filter {
            FactFilter::Current => "Current facts",
            FactFilter::AgentAssertions => "Agent assertions",
            FactFilter::UserConfirmed => "User-confirmed",
            FactFilter::LowConfidence => "Low confidence",
            FactFilter::Stale => "Stale facts",
        };
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(widgets::field_label(theme, "Typed facts"));
        section = section.child(
            widgets::ghost_action(theme)
                .id("memory-fact-filter")
                .text_size(px(11.5))
                .hover(|style| widgets::ghost_hover(theme, style))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.cycle_fact_filter(&project_id, cx)),
                )
                .child(SharedString::from(format!("Filter: {filter_label}"))),
        );
        if self.facts_loading {
            section = section.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Loading memory facts…")),
            );
        } else {
            let visible: Vec<&MemoryFact> = self
                .facts
                .iter()
                .filter(|fact| match self.fact_filter {
                    FactFilter::Current => fact.stale_at_ms.is_none(),
                    FactFilter::AgentAssertions => {
                        fact.source == "agent_assertion" && fact.stale_at_ms.is_none()
                    }
                    FactFilter::UserConfirmed => {
                        (fact.user_confirmed || fact.source == "user_confirmed")
                            && fact.stale_at_ms.is_none()
                    }
                    FactFilter::LowConfidence => {
                        fact.confidence < 0.6 && fact.stale_at_ms.is_none()
                    }
                    FactFilter::Stale => fact.stale_at_ms.is_some(),
                })
                .collect();
            if visible.is_empty() {
                section = section.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("No facts match this filter.")),
                );
            } else {
                for fact in visible {
                    section = section.child(self.fact_row(fact, theme, cx));
                }
            }
        }
        section.into_any_element()
    }

    fn graph_detail(&self, theme: &Theme) -> AnyElement {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(widgets::field_label(theme, "Code graph"));
        if self.graph_files.is_empty() {
            section = section.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "No indexed files yet. A bounded local scan runs when a project session starts.",
                    )),
            );
        } else {
            for file in self.graph_files.iter().take(15) {
                let mut line = format!("{} ·", file.path);
                for neighbor in file.neighbors.iter().take(5) {
                    line.push_str(&format!(
                        " {} {} ({}) ·",
                        neighbor.relation, neighbor.kind, neighbor.label
                    ));
                }
                section = section.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(line)),
                );
            }
        }
        section.into_any_element()
    }

    fn activity_detail(&self, theme: &Theme) -> AnyElement {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(widgets::field_label(theme, "Agent timeline"));
        if self.activity.is_empty() {
            section = section.child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("No recent activity is indexed.")),
            );
        } else {
            for event in self.activity.iter().take(15) {
                let mut line = format!(
                    "{} · {} · {}",
                    format_timestamp(event.occurred_at_ms, &chrono::Local),
                    event.agent,
                    event.kind
                );
                if let Some(tool) = &event.tool_name {
                    line.push_str(&format!(" · {tool}"));
                }
                if let Some(path) = &event.relative_path {
                    line.push_str(&format!(" · {path}"));
                }
                if let Some(outcome) = &event.outcome {
                    line.push_str(&format!(" · {outcome}"));
                }
                section = section.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(line)),
                );
            }
        }
        section.into_any_element()
    }

    fn fact_row(&self, fact: &MemoryFact, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let forget_id = fact.id;
        let promote_id = fact.id;
        let mut details = format!(
            "{} · {} · {}% confidence · {}",
            fact.scope,
            fact.source,
            (fact.confidence * 100.0).round() as u32,
            format_timestamp(fact.valid_from_ms, &chrono::Local)
        );
        if let Some(agent) = &fact.source_agent {
            details.push_str(&format!(" · {agent}"));
        }
        if let Some(path) = &fact.source_path {
            details.push_str(&format!(" · {path}"));
        }
        if let Some(seq) = fact.source_event_seq {
            details.push_str(&format!(" · event {seq}"));
        }
        if let Some(stale_at) = fact.stale_at_ms {
            details.push_str(&format!(
                " · potentially stale since {}",
                format_timestamp(stale_at, &chrono::Local)
            ));
        }
        if let Some(replaced_by) = fact.replaced_by {
            details.push_str(&format!(" · replaced by {replaced_by}"));
        }

        let mut row = div()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .p(px(9.0))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(widgets::row_title(
                theme,
                format!("{} · {}", fact.kind, fact.content),
            ))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(details)),
            )
            .child(
                div().flex().flex_row().gap(px(6.0)).child(
                    widgets::ghost_action(theme)
                        .id(SharedString::from(format!(
                            "memory-forget-fact-{}",
                            fact.id
                        )))
                        .text_size(px(10.5))
                        .text_color(theme.danger)
                        .hover(|style| style.bg(crate::theme::ink(0.06)))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.ask_forget_fact(forget_id, cx)),
                        )
                        .child(SharedString::from("Forget")),
                ),
            );
        if fact.scope != "global" && !fact.user_confirmed {
            row = row.child(
                widgets::ghost_action(theme)
                    .id(SharedString::from(format!(
                        "memory-promote-fact-{}",
                        fact.id
                    )))
                    .text_size(px(10.5))
                    .hover(|style| widgets::ghost_hover(theme, style))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.ask_promote_fact(promote_id, cx)),
                    )
                    .child(SharedString::from("Promote to global…")),
            );
        }
        row.into_any_element()
    }

    fn history_card(
        &self,
        status: &MemoryStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if status.untracked_chats == 0 {
            return None;
        }
        Some(
            widgets::section_card(theme).child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, crate::icons::DOCUMENT))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "History"))
                            .child(widgets::meta_line(
                                theme,
                                vec![SharedString::from(format!(
                                    "{} existing chat journal(s) are not indexed — indexing is opt-in so old sessions never silently become memory.",
                                    status.untracked_chats
                                ))
                                .into_any_element()],
                            )),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id("memory-import-history")
                            .flex_none()
                            .text_size(px(12.0))
                            .hover(|s| widgets::ghost_hover(theme, s))
                            .on_click(cx.listener(|this, _, _, cx| this.import_history(cx)))
                            .child(SharedString::from("Index existing history")),
                    ),
            )
            .into_any_element(),
        )
    }

    fn danger_card(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .child(widgets::row_tile(theme, crate::icons::TRASH_BIN_MINIMALISTIC))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(theme, "Forget all memory"))
                        .child(widgets::meta_line(
                            theme,
                            vec![SharedString::from(
                                "Deletes every retained activity record on this device. Settings and project preferences are kept.",
                            )
                            .into_any_element()],
                        )),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("memory-forget-all")
                        .flex_none()
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .hover(|s| s.bg(crate::theme::ink(0.06)))
                        .on_click(cx.listener(|this, _, _, cx| this.ask_forget_all(cx)))
                        .child(SharedString::from("Forget all")),
                ),
        )
        .into_any_element()
    }
}

impl Render for MemoryPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // Embedded only (Settings → General): the section header and the
        // scroll container belong to the host page, so this renders the
        // column content directly — no page chrome, no nested scroller.
        let mut page = widgets::page_column();

        if let Some(error) = self.error.clone() {
            page = page.child(widgets::error_strip(&theme, error));
        }
        if let Some(notice) = self.notice.clone() {
            page = page.child(widgets::warning_strip(&theme, notice));
        }
        if let Some(strip) = self.confirm_strip(&theme, cx) {
            page = page.child(strip);
        }

        match self.status.ready() {
            Some(status) if status.available => {
                page = page
                    .child(self.storage_card(status, &theme, cx))
                    .child(self.control_card(status, &theme, cx))
                    .child(self.projects_card(status, &theme, cx))
                    .when_some(self.history_card(status, &theme, cx), |el, card| {
                        el.child(card)
                    })
                    .child(self.danger_card(&theme, cx));
            }
            Some(_) => {
                page = page.child(widgets::section_card(&theme).child(
                    widgets::card_row(&theme, true).child(SharedString::from(
                        "Memory is not available in this engine build.",
                    )),
                ));
            }
            None => {
                page = page.child(
                    div()
                        .mt(px(16.0))
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(if self.loading {
                            "Reading memory status…"
                        } else {
                            "Memory status unavailable."
                        })),
                );
            }
        }

        page.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_format() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn cpu_duration_format() {
        assert_eq!(format_cpu_duration(500), "500 ms");
        assert_eq!(format_cpu_duration(1500), "1.5 s");
        assert_eq!(format_cpu_duration(90_000), "1.5 min");
    }

    #[test]
    fn project_label_prefers_root_then_short_id() {
        let project = MemoryProject {
            project_id: "git:abcdef0123456789".into(),
            root_path: Some("/home/u/repos/komet".into()),
            ..Default::default()
        };
        assert_eq!(project_label(&project), "komet");
        let bare = MemoryProject {
            project_id: "git:abcdef0123456789".into(),
            ..Default::default()
        };
        assert!(project_label(&bare).starts_with("git:abcdef01"));
        // A hand-edited multi-byte id must not panic on the slice boundary.
        let multibyte = MemoryProject {
            project_id: "proj-éééééééé".into(),
            ..Default::default()
        };
        assert!(project_label(&multibyte).starts_with("proj-"));
    }

    #[test]
    fn usage_lines_report_week_and_read_file_split() {
        let usage = UsageWeek {
            briefings: 12,
            briefing_chars: 4800,
            memory_calls: vec![
                MemoryCallMetric {
                    metric: "call.memory.read_file".into(),
                    count: 30,
                },
                MemoryCallMetric {
                    metric: "call.memory.search".into(),
                    count: 4,
                },
            ],
            sessions_briefed: 4,
            sessions_unbriefed: 2,
            read_files_briefed: 8,
            read_files_unbriefed: 10,
        };
        let lines = usage_lines(&usage);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("12 briefing(s)"));
        assert!(lines[0].contains("34 memory tool call(s)"));
        assert!(lines[1].contains("briefed 2.0 (4 sessions)"));
        assert!(lines[1].contains("unbriefed 5.0 (2 sessions)"));

        // No sessions yet → only the totals line, no empty comparison.
        let quiet = UsageWeek::default();
        assert_eq!(usage_lines(&quiet).len(), 1);
    }
}
