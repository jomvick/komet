//! Settings → Memory: the local project-memory index (issue #78, phase 1).
//!
//! The index is device-local and allow-listed — it never leaves this machine
//! and never stores prompts, commands, file contents or credentials. This page
//! is the visibility/control surface that makes the default-on choice
//! acceptable: where the data lives, how much there is, per-project switches
//! and exclusions, retention, and explicit forget/import actions.
//!
//! Storage changes apply at next engine start (the live SQLite connections
//! cannot be moved underneath themselves); the page says so instead of
//! pretending the move is instant.

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use serde::Deserialize;

use komet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// Retention choices offered in the UI (days). The engine clamps to its own
/// bounds; these are the values a user actually wants.
const RETENTION_PRESETS: [u32; 5] = [7, 30, 90, 180, 365];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryStatus {
    pub available: bool,
    pub enabled: bool,
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

/// Which destructive action is waiting for its second click.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    ForgetProject(String),
    ForgetAll,
}

pub struct MemoryPage {
    state: Entity<AppState>,
    status: Loadable<MemoryStatus>,
    loading: bool,
    task: Option<Task<()>>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// Draft for a custom storage folder (empty = the default location).
    location_input: Entity<ComposerInput>,
    /// Project whose detail row is expanded, plus its exclusions draft.
    expanded: Option<String>,
    exclusions_input: Option<Entity<ComposerInput>>,
    confirm: Option<Confirm>,
}

impl MemoryPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let location_input = cx.new(|cx| ComposerInput::new("Custom folder…", cx));
        cx.subscribe(&location_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.apply_location(cx);
            }
        })
        .detach();
        let mut page = Self {
            state,
            status: Loadable::Idle,
            loading: false,
            task: None,
            error: None,
            notice: None,
            location_input,
            expanded: None,
            exclusions_input: None,
            confirm: None,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if !matches!(self.status, Loadable::Ready(_)) {
                self.status = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        if matches!(self.status, Loadable::Idle) {
            self.status = Loadable::Loading;
        }
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_MEMORY_STATUS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                page.apply(result, cx);
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
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| match result {
                Ok(_) => {
                    page.error = None;
                    page.confirm = None;
                    page.load(cx);
                }
                Err(err) => {
                    page.error = Some(SharedString::from(err.to_string()));
                    cx.notify();
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
        } else {
            self.expanded = Some(project.project_id.clone());
            let input = cx.new(|cx| ComposerInput::new("Path exclusions (one per line)", cx));
            input.update(cx, |input, cx| {
                input.set_text(project.exclusions.join("\n"), cx)
            });
            self.exclusions_input = Some(input);
        }
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
        let (message, danger_id) = match &confirm {
            Confirm::ForgetProject(project_id) => (
                SharedString::from(format!(
                    "Forget every retained activity record for {}? This cannot be undone.",
                    project_id
                )),
                "memory-forget-project-confirm",
            ),
            Confirm::ForgetAll => (
                SharedString::from(
                    "Forget ALL retained agent memory on this device? This cannot be undone.",
                ),
                "memory-forget-all-confirm",
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
                                .child(SharedString::from("Forget")),
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
        let project_id = project.project_id.clone();
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
                                this.save_exclusions(project_id.clone(), cx)
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
