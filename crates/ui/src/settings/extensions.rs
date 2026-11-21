//! Settings → Extensions: the agent-native inventory — skills, MCP servers
//! and plugins discovered read-only in each agent's own config files — with a
//! per-(agent, item) toggle backed by komet's `agent-ext-overlay.json`.
//!
//! The page never writes agent config: `SetHarnessExtensionEnabled` only
//! records the decision in komet's overlay, which launch-time mediation
//! (OpenCode disable list, Claude `--strict-mcp-config`) applies per agent.

use std::collections::BTreeMap;

use gpui::{Context, Entity, Render, SharedString, Task, Window, div, prelude::*, px};
use komet_harness::ext::{ExtItem, ExtKind};
use komet_proto::HarnessId;
use komet_rpc::methods;
use serde::Deserialize;

use crate::icons;
use crate::settings::providers::harness_display_name;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// One inventory row: the shared `ExtItem` flattened with the engine's
/// per-agent effective enabled map (`{"claude-code": true, …}`).
#[derive(Debug, Clone, Deserialize)]
struct ExtEntry {
    #[serde(flatten)]
    item: ExtItem,
    #[serde(default)]
    enabled: BTreeMap<String, bool>,
    /// Agent slug → reason the row is read-only for that agent.
    #[serde(default)]
    read_only: BTreeMap<String, String>,
    /// Agent slugs with an explicit overlay decision ("modified" marker).
    #[serde(default)]
    modified: Vec<String>,
}

/// A rendered row: one (item, agent) pair.
struct ExtRow {
    /// Stable render id (`ElementId::named_usize` needs a usize).
    uid: usize,
    entry: usize,
    agent: HarnessId,
    agent_slug: String,
    enabled: bool,
    /// `Some(reason)` → no toggle; the reason is shown instead.
    read_only_reason: Option<String>,
    /// Explicit overlay decision exists → offer "Réinitialiser".
    modified: bool,
    /// Short source hint (config file name or skills dir) for the meta line.
    source_hint: String,
}

pub struct ExtensionsPage {
    state: Entity<AppState>,
    entries: Vec<ExtEntry>,
    loaded: bool,
    /// Inventory fetch in flight. Separate from `mutation_task` so a row
    /// toggle cannot cancel an in-flight refresh (or vice versa).
    refresh_task: Option<Task<()>>,
    /// At most one overlay mutation at a time: toggle/reset clicks while a
    /// write is in flight are ignored (controls render disabled).
    mutation_task: Option<Task<()>>,
    status: Option<SharedString>,
    status_is_error: bool,
}

const KINDS: [(ExtKind, &str, &str); 3] = [
    (
        ExtKind::Skill,
        "Skills",
        "Skill folders each agent loads natively.",
    ),
    (
        ExtKind::McpServer,
        "MCP servers",
        "MCP servers declared in agent configs — komet-injected servers stay managed under Integrations.",
    ),
    (
        ExtKind::Plugin,
        "Plugins",
        "Plugins installed in an agent's own plugin store.",
    ),
];

fn kind_icon(kind: ExtKind) -> &'static str {
    match kind {
        ExtKind::Skill => icons::PEN,
        ExtKind::McpServer => icons::PLUG,
        ExtKind::Plugin => icons::WIDGET,
    }
}

fn origin_label(origin: komet_harness::ext::Origin) -> &'static str {
    match origin {
        komet_harness::ext::Origin::External => "agent config",
        komet_harness::ext::Origin::Komet => "komet",
    }
}

impl ExtensionsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            entries: Vec::new(),
            loaded: false,
            refresh_task: None,
            mutation_task: None,
            status: None,
            status_is_error: false,
        };
        page.refresh(cx);
        page
    }

    /// Re-fetch the inventory — called when the section is shown so toggles
    /// made elsewhere (or config edits by the agent itself) show up.
    pub fn on_shown(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // One refresh at a time: a second call would spawn a parallel fetch
        // whose (possibly staler) reply could overwrite a newer one.
        if self.refresh_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            return;
        };
        self.status = None;
        self.status_is_error = false;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HARNESS_EXTENSIONS, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<ExtEntry>>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                page.refresh_task = None;
                match result {
                    Ok(entries) => {
                        page.entries = entries;
                        page.loaded = true;
                    }
                    // List failures and malformed replies surface as an
                    // error strip with a retry — never silently swallowed.
                    Err(error) => page.set_status(error, true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shared mutation flow for toggle (`enabled: Some`) and reset
    /// (`enabled: None`): the overlay write is followed by an inventory
    /// refresh, which is the only place effective state (registry gate,
    /// mediation policy) is reconciled. A failed refresh keeps the last good
    /// list and reports the error — for a plain toggle the clicked state is
    /// still applied locally so the row does not look stuck.
    fn mutate(
        &mut self,
        entry_ix: usize,
        agent_slug: String,
        enabled: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if self.mutation_task.is_some() {
            return;
        }
        let Some(entry) = self.entries.get(entry_ix) else {
            return;
        };
        let params = serde_json::json!({
            "kind": entry.item.kind.as_str(),
            "origin": entry.item.origin.as_str(),
            "harness": agent_slug,
            "id": entry.item.id,
            "enabled": enabled,
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let write = engine
                .client()
                .call(methods::SET_HARNESS_EXTENSION_ENABLED, params)
                .await;
            let list = engine
                .client()
                .call(methods::LIST_HARNESS_EXTENSIONS, serde_json::json!({}))
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<Vec<ExtEntry>>(value).ok());
            this.update(cx, |page, cx| {
                page.mutation_task = None;
                match write {
                    Err(error) => page.set_status(error.to_string(), true),
                    Ok(_) => match list {
                        Some(entries) => {
                            page.entries = entries;
                            page.loaded = true;
                        }
                        None => {
                            // The decision was recorded but the refresh
                            // failed: reflect the toggle locally and say so.
                            if let Some(e) = enabled
                                && let Some(entry) = page.entries.get_mut(entry_ix)
                            {
                                entry.enabled.insert(agent_slug.clone(), e);
                                if !entry.modified.iter().any(|m| m == &agent_slug) {
                                    entry.modified.push(agent_slug.clone());
                                }
                            }
                            page.set_status(
                                "Enregistré, mais l'inventaire n'a pas pu être actualisé.",
                                true,
                            );
                        }
                    },
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle(
        &mut self,
        entry_ix: usize,
        agent_slug: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.mutate(entry_ix, agent_slug, Some(!enabled), cx);
    }

    /// Clear the overlay entry (`enabled: null`); the follow-up inventory
    /// fetch restores the agent-native default.
    fn reset(&mut self, entry_ix: usize, agent_slug: String, cx: &mut Context<Self>) {
        self.mutate(entry_ix, agent_slug, None, cx);
    }

    fn set_status(&mut self, message: impl Into<SharedString>, is_error: bool) {
        self.status = Some(message.into());
        self.status_is_error = is_error;
    }

    /// Flatten entries into (item, agent) rows grouped for rendering.
    fn rows(&self) -> Vec<(ExtKind, Vec<ExtRow>)> {
        let mut grouped: Vec<(ExtKind, Vec<ExtRow>)> = KINDS
            .iter()
            .map(|(kind, _, _)| (*kind, Vec::new()))
            .collect();
        let mut uid = 0usize;
        for (ix, entry) in self.entries.iter().enumerate() {
            for (slug, enabled) in &entry.enabled {
                let Some(agent) = HarnessId::from_slug(slug) else {
                    continue;
                };
                let source_hint = entry
                    .item
                    .sources
                    .iter()
                    .find(|s| s.agent == agent)
                    .and_then(|s| s.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                let kind_ix = KINDS
                    .iter()
                    .position(|(kind, _, _)| *kind == entry.item.kind)
                    .unwrap_or(0);
                grouped[kind_ix].1.push(ExtRow {
                    uid,
                    entry: ix,
                    agent,
                    agent_slug: slug.clone(),
                    enabled: *enabled,
                    read_only_reason: entry.read_only.get(slug).cloned(),
                    modified: entry.modified.iter().any(|m| m == slug),
                    source_hint,
                });
                uid += 1;
            }
        }
        grouped
    }
}

impl Render for ExtensionsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let rows = self.rows();
        let loaded = self.loaded;
        let mutating = self.mutation_task.is_some();
        let status = self.status.clone();
        let status_is_error = self.status_is_error;
        let total: usize = rows.iter().map(|(_, r)| r.len()).sum();

        div()
            .id("extensions-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Extensions", Some(total)))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Skills, MCP servers and plugins found in each agent's own config. Toggles are stored by komet — agent files are never modified.",
                    ))
                    .when_some(status, |page, message| {
                        page.child(if status_is_error {
                            widgets::error_strip(&theme, message)
                        } else {
                            widgets::warning_strip(&theme, message)
                        })
                        // Any surfaced failure gets an explicit retry: the
                        // inventory fetch is idempotent and cheap.
                        .when(status_is_error, |strip| {
                            strip.child(
                                div()
                                    .id("ext-retry")
                                    .flex_none()
                                    .mt(px(2.0))
                                    .text_size(px(11.5))
                                    .text_color(theme.accent)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme.text))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.refresh(cx);
                                    }))
                                    .child("Réessayer"),
                            )
                        })
                    })
                    .when(loaded && total == 0, |page| {
                        page.child(
                            widgets::section_card(&theme).child(
                                div()
                                    .px(px(20.0))
                                    .py(px(18.0))
                                    .text_size(px(13.0))
                                    .text_color(theme.text_muted)
                                    .child("No extensions found in agent configs."),
                            ),
                        )
                    })
                    .children(rows.into_iter().filter(|(_, rows)| !rows.is_empty()).map(
                        |(kind, rows)| {
                            let (_, title, subtitle) = KINDS
                                .iter()
                                .find(|(k, _, _)| *k == kind)
                                .copied()
                                .unwrap();
                            let icon_path = kind_icon(kind);
                            let count = rows.len();
                            let card = widgets::section_card(&theme).children(
                                rows.iter().enumerate().map(|(ix, row)| {
                                    let entry = &self.entries[row.entry];
                                    let enabled = row.enabled;
                                    let entry_ix = row.entry;
                                    let agent_slug = row.agent_slug.clone();
                                    let reset_slug = agent_slug.clone();
                                    let agent_name = harness_display_name(row.agent);
                                    let origin = origin_label(entry.item.origin);
                                    let source_hint = row.source_hint.clone();
                                    let name = entry.item.name.clone();
                                    let detail = entry.item.detail.clone();
                                    let read_only_reason = row.read_only_reason.clone();
                                    let modified = row.modified;
                                    let mut meta_parts = vec![
                                        agent_name.to_string(),
                                        origin.to_string(),
                                    ];
                                    if !source_hint.is_empty() {
                                        meta_parts.push(source_hint);
                                    }
                                    if let Some(detail) = detail
                                        && !detail.is_empty()
                                    {
                                        meta_parts.push(detail);
                                    }
                                    if let Some(reason) = &read_only_reason {
                                        meta_parts.push(reason.clone());
                                    }
                                    let meta = meta_parts.join("  ·  ");
                                    widgets::card_row(&theme, ix == 0)
                                        .child(widgets::row_tile(&theme, icon_path))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .child(widgets::row_title(&theme, name))
                                                .child(
                                                    div()
                                                        .text_size(px(11.5))
                                                        .text_color(theme.text_muted)
                                                        .child(SharedString::from(meta)),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap(px(10.0))
                                                .when(modified, |el| {
                                                    let reset = div()
                                                        .id(gpui::ElementId::named_usize(
                                                            "ext-reset",
                                                            row.uid,
                                                        ))
                                                        .text_size(px(11.0))
                                                        .text_color(theme.accent)
                                                        .cursor_pointer()
                                                        .hover(|s| {
                                                            s.text_color(theme.text)
                                                        })
                                                        .child("Réinitialiser");
                                                    // Mutations are
                                                    // serialized: while one
                                                    // is in flight the
                                                    // control renders
                                                    // inert instead of
                                                    // dropping the click.
                                                    if mutating {
                                                        el.child(reset.opacity(0.4))
                                                    } else {
                                                        el.child(reset.on_click(
                                                            cx.listener(
                                                                move |this, _, _, cx| {
                                                                    cx.stop_propagation();
                                                                    this.reset(
                                                                        entry_ix,
                                                                        reset_slug.clone(),
                                                                        cx,
                                                                    );
                                                                },
                                                            ),
                                                        ))
                                                    }
                                                })
                                                // Read-only rows show the
                                                // reason in the meta line;
                                                // the toggle is omitted so
                                                // nothing implies a no-op.
                                                .when(read_only_reason.is_none(), |el| {
                                                    let toggle = widgets::toggle_switch(
                                                        &theme, enabled,
                                                    )
                                                    .id(gpui::ElementId::named_usize(
                                                        "ext-toggle",
                                                        row.uid,
                                                    ));
                                                    if mutating {
                                                        el.child(toggle.opacity(0.4))
                                                    } else {
                                                        el.child(
                                                            toggle
                                                                .cursor_pointer()
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        cx.stop_propagation();
                                                                        this.toggle(
                                                                            entry_ix,
                                                                            agent_slug.clone(),
                                                                            enabled,
                                                                            cx,
                                                                        );
                                                                    },
                                                                )),
                                                        )
                                                    }
                                                }),
                                        )
                                        .into_any_element()
                                }),
                            );
                            div()
                                .child(widgets::section_header(
                                    &theme,
                                    title,
                                    format!(
                                        "{count} item{} — {subtitle}",
                                        if count == 1 { "" } else { "s" }
                                    ),
                                ))
                                .child(card)
                                .into_any_element()
                        },
                    )),
            )
    }
}
