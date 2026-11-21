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
    busy: bool,
    status: Option<SharedString>,
    status_is_error: bool,
    task: Option<Task<()>>,
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
            busy: false,
            status: None,
            status_is_error: false,
            task: None,
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
        if self.busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            return;
        };
        self.busy = true;
        self.status = None;
        self.status_is_error = false;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HARNESS_EXTENSIONS, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<ExtEntry>>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(entries) => {
                        page.entries = entries;
                        page.loaded = true;
                    }
                    Err(error) => page.set_status(error, true),
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
        let Some(entry) = self.entries.get(entry_ix) else {
            return;
        };
        let params = serde_json::json!({
            "kind": entry.item.kind.as_str(),
            "harness": agent_slug,
            "id": entry.item.id,
            "enabled": !enabled,
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        let next = !enabled;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_HARNESS_EXTENSION_ENABLED, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        if let Some(entry) = page.entries.get_mut(entry_ix) {
                            entry.enabled.insert(agent_slug.clone(), next);
                            if !entry.modified.iter().any(|m| m == &agent_slug) {
                                entry.modified.push(agent_slug);
                            }
                        }
                    }
                    Err(error) => page.set_status(error.to_string(), true),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Clear the overlay entry (`enabled: null`) then re-fetch the inventory
    /// so the row shows the agent-native default again.
    fn reset(&mut self, entry_ix: usize, agent_slug: String, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(entry_ix) else {
            return;
        };
        let params = serde_json::json!({
            "kind": entry.item.kind.as_str(),
            "harness": agent_slug,
            "id": entry.item.id,
            "enabled": serde_json::Value::Null,
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.set_status("The local engine is unavailable.", true);
            cx.notify();
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SET_HARNESS_EXTENSION_ENABLED, params)
                .await
                .map(|_| ());
            // Re-fetch regardless of the clear outcome's shape: the list is
            // the source of truth for effective state.
            let list = engine
                .client()
                .call(methods::LIST_HARNESS_EXTENSIONS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                if let Err(error) = result {
                    page.set_status(error.to_string(), true);
                }
                if let Ok(value) = list
                    && let Ok(entries) = serde_json::from_value::<Vec<ExtEntry>>(value)
                {
                    page.entries = entries;
                    page.loaded = true;
                }
                cx.notify();
            })
            .ok();
        }));
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
        let busy = self.busy;
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
                    })
                    .when(!loaded && !busy, |page| {
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
                                                    el.child(
                                                        div()
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
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    cx.stop_propagation();
                                                                    this.reset(
                                                                        entry_ix,
                                                                        reset_slug.clone(),
                                                                        cx,
                                                                    );
                                                                },
                                                            ))
                                                            .child("Réinitialiser"),
                                                    )
                                                })
                                                // Read-only rows show the
                                                // reason in the meta line;
                                                // the toggle is omitted so
                                                // nothing implies a no-op.
                                                .when(read_only_reason.is_none(), |el| {
                                                    el.child(
                                                        widgets::toggle_switch(
                                                            &theme, enabled,
                                                        )
                                                        .id(gpui::ElementId::named_usize(
                                                            "ext-toggle",
                                                            row.uid,
                                                        ))
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
