//! Browser surface (issue #45): the right-pane home of the browser-use
//! preset. Shows the Chrome / npx probe from `BrowserUseStatus`, the
//! readable blocker when enabling is impossible, and the enable / disable
//! control for the pinned `chrome-devtools-mcp` server. Device-local, like
//! the MCP registry it fronts.

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use komet_rpc::methods;
use serde::Deserialize;

use crate::icons::{self, icon};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BrowserUseStatus {
    pub chrome_installed: bool,
    pub chrome_path: Option<String>,
    pub npx_installed: bool,
    pub configured: bool,
    pub enabled: bool,
    pub always_load: bool,
    pub package: String,
    pub blocker: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusReply {
    status: BrowserUseStatus,
}

pub struct BrowserPanel {
    state: Entity<AppState>,
    status: Option<BrowserUseStatus>,
    error: Option<SharedString>,
    loaded: bool,
    busy: bool,
    task: Option<Task<()>>,
}

impl BrowserPanel {
    pub fn new(state: Entity<AppState>, _cx: &mut Context<Self>) -> Self {
        Self {
            state,
            status: None,
            error: None,
            loaded: false,
            busy: false,
            task: None,
        }
    }

    pub fn tab_title(&self) -> SharedString {
        "Browser".into()
    }

    /// First render (and every re-open) refreshes the probe — Chrome or Node
    /// may have been installed since.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        self.call(methods::BROWSER_USE_STATUS, serde_json::json!({}), cx);
    }

    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            let _ = this.update(cx, |panel, cx| {
                panel.busy = false;
                match result {
                    Ok(value) => match serde_json::from_value::<StatusReply>(value) {
                        Ok(reply) => panel.status = Some(reply.status),
                        Err(err) => panel.error = Some(format!("{method}: {err}").into()),
                    },
                    Err(err) => panel.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.call(methods::BROWSER_USE_STATUS, serde_json::json!({}), cx);
    }

    fn enable(&mut self, cx: &mut Context<Self>) {
        self.call(methods::ENABLE_BROWSER_USE, serde_json::json!({}), cx);
    }

    fn disable(&mut self, cx: &mut Context<Self>) {
        self.call(methods::DISABLE_BROWSER_USE, serde_json::json!({}), cx);
    }

    fn set_always_load(&mut self, always_load: bool, cx: &mut Context<Self>) {
        self.call(
            methods::ENABLE_BROWSER_USE,
            serde_json::json!({ "alwaysLoad": always_load }),
            cx,
        );
    }

    /// One probe / state row in the settings-card idiom: tile + title +
    /// wrapping meta line, trailing badge or control.
    fn row(
        theme: &Theme,
        first: bool,
        icon_path: &'static str,
        title: &str,
        meta: Vec<AnyElement>,
        trailing: AnyElement,
    ) -> gpui::Div {
        widgets::card_row(theme, first)
            .child(widgets::row_tile(theme, icon_path))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, title.to_string()))
                    .when(!meta.is_empty(), |el| {
                        el.child(widgets::meta_line(theme, meta))
                    }),
            )
            .child(div().flex_none().child(trailing))
    }

    fn meta(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
        div()
            .min_w_0()
            .text_color(theme.text_muted.opacity(0.65))
            .child(text.into())
            .into_any_element()
    }

    fn probe_badge(theme: &Theme, ok: bool, ok_label: &str, missing_label: &str) -> AnyElement {
        if ok {
            widgets::badge_active(theme, ok_label.to_string())
        } else {
            widgets::badge(theme, missing_label.to_string()).text_color(theme.danger_muted)
        }
        .into_any_element()
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let status = self.status.clone().unwrap_or_default();
        let have_status = self.status.is_some();
        let busy = self.busy;
        let blocked = status.blocker.is_some() && !status.enabled;
        let can_toggle = have_status && !busy && (status.enabled || !blocked);

        let header = widgets::page_header(&theme, "Browser", None);
        let subtitle = widgets::page_subtitle(
            &theme,
            "Headless Chrome for agents via chrome-devtools-mcp: navigate, snapshot, click, fill and read console errors.",
        );

        // Probes: Chrome and npx, each with a status pill; details wrap.
        let mut probes = widgets::section_card(&theme);
        if !have_status && self.error.is_none() {
            probes = probes.child(
                widgets::card_row(&theme, true).child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Probing Chrome and npx…")),
                ),
            );
        } else {
            let chrome_meta = match status.chrome_path.as_deref() {
                Some(path) => vec![Self::meta(&theme, path.to_string())],
                None => vec![Self::meta(
                    &theme,
                    format!("not found — install Chrome or set {}", "CHROME_PATH"),
                )],
            };
            probes = probes
                .child(Self::row(
                    &theme,
                    true,
                    icons::GLOBE,
                    "Chrome / Chromium",
                    chrome_meta,
                    Self::probe_badge(&theme, status.chrome_installed, "Found", "Missing"),
                ))
                .child(Self::row(
                    &theme,
                    false,
                    icons::TERMINAL,
                    "npx (Node.js)",
                    vec![Self::meta(
                        &theme,
                        if status.npx_installed {
                            format!("runs {}", status.package)
                        } else {
                            "not found — install Node.js ≥ 20".to_string()
                        },
                    )],
                    Self::probe_badge(&theme, status.npx_installed, "Found", "Missing"),
                ));
        }

        // Controls: enable toggle, always-on toggle, re-check.
        let enable_meta = if status.enabled {
            "Registered as the chrome-devtools MCP server"
        } else {
            "Registers chrome-devtools-mcp for Claude Code, OpenCode, Grok, Hermes, Pi and Cline"
        };
        let enable_toggle = widgets::toggle_switch(&theme, status.enabled)
            .id("browser-enable")
            .when(!can_toggle, |el| el.opacity(0.5))
            .when(can_toggle, |el| {
                let enabled = status.enabled;
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if enabled {
                            this.disable(cx)
                        } else {
                            this.enable(cx)
                        }
                    }))
            });
        let always_on = status.enabled && !busy;
        let always_toggle = widgets::toggle_switch(&theme, status.always_load)
            .id("browser-always-load")
            .when(!always_on, |el| el.opacity(0.5))
            .when(always_on, |el| {
                let next = !status.always_load;
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.set_always_load(next, cx)))
            });
        let recheck = widgets::ghost_action(&theme)
            .id("browser-refresh")
            .hover(|s| widgets::ghost_hover(&theme, s))
            .when(busy, |el| el.opacity(0.5))
            .when(!busy, |el| {
                el.on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
            })
            .child(
                icon(icons::REFRESH)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from("Re-check"));

        let controls = widgets::section_card(&theme)
            .child(Self::row(
                &theme,
                true,
                icons::CHECK,
                "Enable browser use",
                vec![Self::meta(&theme, enable_meta)],
                enable_toggle.into_any_element(),
            ))
            .child(Self::row(
                &theme,
                false,
                icons::INFO_CIRCLE,
                "Always on",
                vec![Self::meta(
                    &theme,
                    "Attach to every compatible run instead of per-chat assignment",
                )],
                always_toggle.into_any_element(),
            ))
            .child(
                widgets::card_row(&theme, false)
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from(
                                "Probes are file checks only; Chrome is never launched here.",
                            )),
                    )
                    .child(recheck),
            );

        let mut page = widgets::page_column()
            .child(header)
            .child(subtitle)
            .child(probes);
        if let Some(blocker) = status.blocker.clone().filter(|_| blocked) {
            page = page.child(widgets::warning_strip(&theme, blocker));
        }
        if let Some(error) = self.error.clone() {
            page = page.child(widgets::error_strip(&theme, error));
        }
        page = page.child(controls);

        // Right pane is narrow: tighten the settings column's side padding.
        div()
            .id("browser-page")
            .size_full()
            .overflow_y_scroll()
            .child(page.px(px(16.0)).pt(px(20.0)))
    }
}
