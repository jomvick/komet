//! Settings → Memory: local project memory (issue #78, phases 1–7).
//!
//! The index is device-local and allow-listed — it never leaves this machine
//! and never stores prompts, commands, file contents or credentials. This page
//! is the visibility/control surface that makes the default-on choice
//! acceptable: where the data lives, how much there is, per-project switches
//! and exclusions, retention, typed facts, graph/timeline inspection,
//! explicit forget actions, and one-way Obsidian export.
//!
//! Storage changes apply at next engine start (the live SQLite connections
//! cannot be moved underneath themselves); the page says so instead of
//! pretending the move is instant.

use gpui::{AnyElement, App, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use serde::Deserialize;

use komet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// Retention choices offered in the UI (days). The engine clamps to its own
/// bounds; these are the values a user actually wants.
const RETENTION_PRESETS: [u32; 5] = [7, 30, 90, 180, 365];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryStatus {
    pub available: bool,
    pub enabled: bool,
    pub embeddings_enabled: bool,
    pub embeddings_indexed: u64,
    pub embeddings_total: u64,
    pub embeddings_working: bool,
    pub embeddings_error: Option<String>,
    pub process_cpu_ms: Option<u64>,
    pub process_peak_rss_bytes: Option<u64>,
    pub retention_days: u32,
    pub dir: Option<String>,
    pub pending_dir: Option<String>,
    pub restart_required: bool,
    pub events: u64,
    pub chats: u64,
    pub bytes: u64,
    pub fts: bool,
    pub untracked_chats: u64,
    pub projects: Vec<MemoryProject>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySemanticResult {
    pub fact: MemoryFact,
    pub distance: f32,
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
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// The engine the displayed status belongs to — memory is device-local,
    /// so a workspace/device switch must reload, never show the old index.
    engine: Option<EngineHandle>,
    /// Bumped on every load AND every engine rebind: async results carrying
    /// an older generation are dropped, so a slow reply from the previous
    /// engine can't overwrite the new one's status.
    generation: u64,
    /// Draft for a custom storage folder (empty = the default location).
    location_input: Entity<ComposerInput>,
    export_input: Entity<ComposerInput>,
    semantic_query_input: Entity<ComposerInput>,
    /// Project whose detail row is expanded, plus its exclusions draft.
    expanded: Option<String>,
    exclusions_input: Option<Entity<ComposerInput>>,
    facts: Vec<MemoryFact>,
    semantic_results: Vec<MemorySemanticResult>,
    semantic_loading: bool,
    semantic_generation: u64,
    graph_files: Vec<MemoryGraphFile>,
    activity: Vec<MemoryActivity>,
    facts_loading: bool,
    facts_generation: u64,
    fact_filter: FactFilter,
    confirm: Option<Confirm>,
}

impl MemoryPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let location_input = cx.new(|cx| ComposerInput::new("Custom folder…", cx));
        let export_input = cx.new(|cx| ComposerInput::new("New or Komet-owned export folder…", cx));
        let semantic_query_input = cx.new(|cx| ComposerInput::new("Search project memory…", cx));
        cx.subscribe(&location_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.apply_location(cx);
            }
        })
        .detach();
        cx.subscribe(&semantic_query_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.search_semantic(cx);
            }
        })
        .detach();
        cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx))
            .detach();
        let mut page = Self {
            state,
            status: Loadable::Idle,
            loading: false,
            task: None,
            error: None,
            notice: None,
            engine: None,
            generation: 0,
            location_input,
            export_input,
            semantic_query_input,
            expanded: None,
            exclusions_input: None,
            facts: Vec::new(),
            semantic_results: Vec::new(),
            semantic_loading: false,
            semantic_generation: 0,
            graph_files: Vec::new(),
            activity: Vec::new(),
            facts_loading: false,
            facts_generation: 0,
            fact_filter: FactFilter::Current,
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
        self.semantic_results.clear();
        self.semantic_loading = false;
        self.semantic_generation += 1;
        self.graph_files.clear();
        self.activity.clear();
        self.facts_loading = false;
        self.facts_generation += 1;
        self.fact_filter = FactFilter::Current;
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
        cx: &mut Context<Self>,
    ) {
        self.status = match result.map_err(|err| err.to_string()).and_then(|value| {
            serde_json::from_value::<MemoryStatus>(value).map_err(|e| e.to_string())
        }) {
            Ok(status) => {
                // Seed the location draft from the pending (not-yet-applied)
                // value when there is one, else the live directory — but only
                // when the field is empty or still shows the previous value,
                // so a reload never clobbers something the user is typing.
                let draft = status
                    .pending_dir
                    .clone()
                    .or_else(|| status.dir.clone())
                    .unwrap_or_default();
                let current = self.location_input.read(cx).text().trim().to_string();
                let previous = self
                    .status
                    .ready()
                    .and_then(|old| old.pending_dir.clone().or_else(|| old.dir.clone()))
                    .unwrap_or_default();
                if current.is_empty() || current == previous {
                    self.location_input
                        .update(cx, |input, cx| input.set_text(draft, cx));
                }
                Loadable::Ready(status)
            }
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

    fn toggle_embeddings(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.status.ready() else {
            return;
        };
        self.call(
            methods::SET_MEMORY_EMBEDDINGS_ENABLED,
            serde_json::json!({ "enabled": !status.embeddings_enabled }),
            cx,
        );
    }

    fn retry_embeddings(&mut self, cx: &mut Context<Self>) {
        self.call(methods::RETRY_MEMORY_EMBEDDINGS, serde_json::json!({}), cx);
    }

    fn search_semantic(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.expanded.clone() else {
            self.notice = Some(SharedString::from(
                "Open a project below before searching its semantic memory.",
            ));
            cx.notify();
            return;
        };
        let query = self.semantic_query_input.read(cx).text().trim().to_string();
        if query.is_empty() {
            self.notice = Some(SharedString::from("Enter a search query first."));
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some(SharedString::from("Engine not connected."));
            cx.notify();
            return;
        };
        self.notice = None;
        self.semantic_results.clear();
        self.semantic_loading = true;
        self.semantic_generation += 1;
        let generation = self.semantic_generation;
        let params = self.with_target(
            serde_json::json!({ "projectId": project_id, "query": query }),
            cx,
        );
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SEARCH_MEMORY_SEMANTIC, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    value
                        .get("results")
                        .cloned()
                        .ok_or_else(|| "semantic search reply omitted results".to_string())
                })
                .and_then(|value| {
                    serde_json::from_value::<Vec<MemorySemanticResult>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |page, cx| {
                if page.semantic_generation != generation {
                    return;
                }
                page.semantic_loading = false;
                match result {
                    Ok(results) => {
                        page.error = None;
                        page.semantic_results = results;
                    }
                    Err(error) => page.error = Some(SharedString::from(error)),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_retention(&mut self, days: u32, cx: &mut Context<Self>) {
        self.call(
            methods::SET_MEMORY_RETENTION_DAYS,
            serde_json::json!({ "days": days }),
            cx,
        );
    }

    fn apply_location(&mut self, cx: &mut Context<Self>) {
        let draft = self.location_input.read(cx).text().trim().to_string();
        if draft.is_empty() {
            // Empty = back to the default location.
            self.call(
                methods::SET_MEMORY_LOCATION,
                serde_json::json!({ "path": null }),
                cx,
            );
            return;
        }
        if looks_risky_location(&draft) {
            self.error = Some(SharedString::from(
                "That folder looks like a synced or network drive — SQLite in WAL mode can corrupt there. Pick a local folder instead.",
            ));
            cx.notify();
            return;
        }
        self.call(
            methods::SET_MEMORY_LOCATION,
            serde_json::json!({ "path": draft }),
            cx,
        );
    }

    fn reset_location(&mut self, cx: &mut Context<Self>) {
        self.call(
            methods::SET_MEMORY_LOCATION,
            serde_json::json!({ "path": null }),
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
            self.semantic_results.clear();
            self.semantic_generation += 1;
            self.facts_generation += 1;
        } else {
            self.expanded = Some(project.project_id.clone());
            self.semantic_results.clear();
            self.semantic_generation += 1;
            let input = cx.new(|cx| ComposerInput::new("Path exclusions (one per line)", cx));
            input.update(cx, |input, cx| {
                input.set_text(project.exclusions.join("\n"), cx)
            });
            self.exclusions_input = Some(input);
            self.load_facts(&project.project_id, cx);
        }
        cx.notify();
    }

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
        self.task = Some(cx.spawn(async move |this, cx| {
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
        self.task = Some(cx.spawn(async move |this, cx| {
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

/// Synced/network folders can corrupt a WAL-mode SQLite database — warn
/// rather than refuse (the user may know their mount is fine).
fn looks_risky_location(path: &str) -> bool {
    let lower = path.to_lowercase();
    const NEEDLES: &[&str] = &[
        "dropbox",
        "google drive",
        "googledrive",
        "icloud",
        "onedrive",
        "nextcloud",
        "box sync",
        "\\\\",
        "//",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
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
    let seconds = ms / 1000;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
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
            let id = &project.project_id;
            let short = id.len().min(12);
            format!("{}…", &id[..short])
        })
}

impl MemoryPage {
    fn confirm_strip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.confirm.clone()?;
        let (message, danger_id, action_label) = match &confirm {
            Confirm::ForgetProject(project_id) => (
                SharedString::from(format!(
                    "Forget every retained activity record for {}? This cannot be undone.",
                    project_id
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
                .child(
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
                ),
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
        card = card.child(widgets::card_row(theme, false).child(widgets::meta_line(
            theme,
            vec![SharedString::from(
                "Resource guardrails: one background scan per project, at most 5,000 workspace entries and 512 KiB per source file, with one file parsed at a time. No continuous graph simulation.",
            )
            .into_any_element()],
        )));
        let process_usage = match (status.process_cpu_ms, status.process_peak_rss_bytes) {
            (Some(cpu_ms), Some(rss_bytes)) => format!(
                "Komet engine lifetime: {} CPU · {} peak RAM",
                format_cpu_duration(cpu_ms),
                format_bytes(rss_bytes)
            ),
            _ => "Komet engine CPU/RAM metrics unavailable on this device.".into(),
        };
        card = card.child(widgets::card_row(theme, false).child(widgets::meta_line(
            theme,
            vec![SharedString::from(format!(
                "{process_usage}. Disk usage includes the memory database, WAL files, and local model cache; CPU/RAM are engine-wide, not attributed only to memory."
            ))
            .into_any_element()],
        )));
        if status.restart_required
            && let Some(pending) = &status.pending_dir
        {
            card = card.child(widgets::card_row(theme, false).child(widgets::meta_line(
                theme,
                vec![SharedString::from(format!(
                        "Moves to {pending} on next start — existing data is copied over."
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
        let mut card = widgets::section_card(theme)
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
                                    "Local activity index shared by agents on this device. Turning it off keeps the data but stops recording and briefings.",
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
            .child(
                widgets::card_row(theme, false)
                    .child(widgets::row_tile(theme, crate::icons::HARD_DRIVE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Retention"))
                            .child(widgets::meta_line(
                                theme,
                                vec![SharedString::from("Raw activity older than this is pruned.")
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .children(RETENTION_PRESETS.map(|days| {
                                let active = status.retention_days == days;
                                widgets::ghost_action(theme)
                                    .id(SharedString::from(format!("memory-retention-{days}")))
                                    .flex_none()
                                    .text_size(px(11.5))
                                    .when(active, |el| {
                                        el.bg(crate::theme::ink(0.06)).text_color(theme.text)
                                    })
                                    .when(!active, |el| {
                                        el.hover(|s| widgets::ghost_hover(theme, s))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_retention(days, cx)
                                    }))
                                    .child(SharedString::from(format!("{days}d")))
                                    .into_any_element()
                            })),
                    ),
            );

        // Custom location row: draft input + apply/reset.
        let draft = self.location_input.clone();
        let risky = looks_risky_location(draft.read(cx).text());
        card = card.child(
            div()
                .px(px(20.0))
                .py(px(10.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(widgets::field_label(theme, "Storage location"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(draft),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("memory-location-apply")
                                .flex_none()
                                .text_size(px(12.0))
                                .hover(|s| widgets::ghost_hover(theme, s))
                                .on_click(cx.listener(|this, _, _, cx| this.apply_location(cx)))
                                .child(SharedString::from("Apply")),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("memory-location-reset")
                                .flex_none()
                                .text_size(px(12.0))
                                .hover(|s| widgets::ghost_hover(theme, s))
                                .on_click(cx.listener(|this, _, _, cx| this.reset_location(cx)))
                                .child(SharedString::from("Default")),
                        ),
                )
                .child(widgets::meta_line(
                    theme,
                    vec![SharedString::from(
                        "Changes apply on next start — the live database cannot be moved while open.",
                    )
                    .into_any_element()],
                ))
                .when(risky, |el| {
                    el.child(widgets::warning_strip(
                        theme,
                        "That path looks like a synced or network folder. SQLite WAL databases can corrupt there — prefer a local folder.",
                    ))
                }),
        );
        card.into_any_element()
    }

    fn embeddings_card(
        &self,
        status: &MemoryStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query_input = self.semantic_query_input.clone();
        let mut card = widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, crate::icons::EYE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Semantic memory"))
                            .child(widgets::meta_line(
                                theme,
                                vec![SharedString::from(if status.embeddings_working {
                                    format!(
                                        "Indexing {} of {} facts locally…",
                                        status.embeddings_indexed, status.embeddings_total
                                    )
                                } else {
                                    format!(
                                        "{} of {} facts indexed · local model, no network search",
                                        status.embeddings_indexed, status.embeddings_total
                                    )
                                })
                                .into_any_element()],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(theme, status.embeddings_enabled)
                            .id("memory-embeddings-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_embeddings(cx))),
                    ),
            )
            .child(widgets::card_row(theme, false).child(widgets::meta_line(
                theme,
                vec![SharedString::from(
                    "Optional local model (~23 MB), downloaded only after opt-in. Embedding batches pause during agent runs; FTS and graph remain available when off.",
                )
                .into_any_element()],
            )));

        if status.embeddings_enabled {
            card = card.child(
                div()
                    .px(px(20.0))
                    .py(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(widgets::field_label(
                        theme,
                        "Semantic search (open a project below first)",
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().flex_1().min_w_0().child(query_input))
                            .child(
                                widgets::ghost_action(theme)
                                    .id("memory-semantic-search")
                                    .flex_none()
                                    .text_size(px(12.0))
                                    .hover(|s| widgets::ghost_hover(theme, s))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.search_semantic(cx)),
                                    )
                                    .child(SharedString::from(if self.semantic_loading {
                                        "Searching…"
                                    } else {
                                        "Search"
                                    })),
                            )
                            .child(
                                widgets::ghost_action(theme)
                                    .id("memory-embeddings-refresh")
                                    .flex_none()
                                    .text_size(px(12.0))
                                    .hover(|s| widgets::ghost_hover(theme, s))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.retry_embeddings(cx)),
                                    )
                                    .child(SharedString::from("Refresh")),
                            ),
                    ),
            );
            if let Some(error) = &status.embeddings_error {
                card = card.child(widgets::card_row(theme, false).child(widgets::error_strip(
                    theme,
                    SharedString::from(error.clone()),
                )));
            }
            for result in &self.semantic_results {
                card = card.child(
                    widgets::card_row(theme, false).child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(theme.text)
                                    .child(SharedString::from(result.fact.content.clone())),
                            )
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    SharedString::from(format!(
                                        "{} · distance {:.3} · confidence {:.0}%",
                                        result.fact.source,
                                        result.distance,
                                        result.fact.confidence * 100.0
                                    ))
                                    .into_any_element(),
                                ],
                            )),
                    ),
                );
            }
            if !self.semantic_loading && self.semantic_results.is_empty() {
                card = card.child(widgets::card_row(theme, false).child(widgets::meta_line(
                    theme,
                    vec![SharedString::from(
                        "Select a project, enter a query, and search. Query text is embedded locally and is not stored.",
                    )
                    .into_any_element()],
                )));
            }
        } else if let Some(error) = &status.embeddings_error {
            card = card.child(widgets::card_row(theme, false).child(widgets::error_strip(
                theme,
                SharedString::from(error.clone()),
            )));
        }
        card.into_any_element()
    }

    fn projects_card(
        &self,
        status: &MemoryStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut card = widgets::section_card(theme);
        if status.projects.is_empty() {
            card = card.child(widgets::card_row(theme, true).child(SharedString::from(
                "No projects indexed yet — memory appears here after the first agent session.",
            )));
            return card.into_any_element();
        }
        for (index, project) in status.projects.iter().enumerate() {
            card = card.child(self.project_row(project, index == 0, theme, cx));
            if self.expanded.as_deref() == Some(project.project_id.as_str()) {
                card = card.child(self.project_detail(project, theme, cx));
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
                    event.occurred_at_ms, event.agent, event.kind
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
            "{} · {} · {}% confidence · valid from {}",
            fact.scope,
            fact.source,
            (fact.confidence * 100.0).round() as u32,
            fact.valid_from_ms
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
            details.push_str(&format!(" · potentially stale since {stale_at}"));
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
        let mut page = widgets::page_column()
            .id("memory-page")
            .child(widgets::page_header(&theme, "Memory", None))
            .child(widgets::page_subtitle(
                &theme,
                "A local activity index shared by agents on this device — what ran, which files were touched, and how sessions ended. It stores metadata only: never prompts, commands, file contents or credentials, and it never leaves this machine.",
            ));

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
                    .child(self.embeddings_card(status, &theme, cx))
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

        div()
            .id("memory-page-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_locations_flagged() {
        assert!(looks_risky_location("/home/u/Dropbox/memory"));
        assert!(looks_risky_location("/Volumes/Google Drive/memory"));
        assert!(looks_risky_location("\\\\nas\\share\\memory"));
        assert!(!looks_risky_location("/home/u/.local/share/komet/memory"));
        assert!(!looks_risky_location("/home/u/skyline-market-data"));
    }

    #[test]
    fn bytes_format() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn cpu_duration_format() {
        assert_eq!(format_cpu_duration(59_000), "59s");
        assert_eq!(format_cpu_duration(65_000), "1m 5s");
        assert_eq!(format_cpu_duration(3_661_000), "1h 1m");
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
    }
}
