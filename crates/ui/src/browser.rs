//! Browser surface (issue #45): the right-pane home of the browser-use
//! preset. Shows the Chrome / npx probe from `BrowserUseStatus`, the
//! readable blocker when enabling is impossible, and the enable / disable
//! control for the pinned `chrome-devtools-mcp` server. Device-local, like
//! the MCP registry it fronts.

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use komet_rpc::methods;
use serde::Deserialize;

use crate::icons::{self, icon};
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

    fn probe_row(theme: &Theme, ok: bool, label: &str, detail: Option<&str>) -> AnyElement {
        let dot = div().size(px(8.0)).flex_none().rounded_full().bg(if ok {
            theme.success
        } else {
            theme.danger
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(dot)
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(SharedString::from(label.to_string())),
            )
            .when_some(detail, |el, detail| {
                el.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(SharedString::from(detail.to_string())),
                )
            })
            .into_any_element()
    }

    fn button(
        id: &'static str,
        theme: &Theme,
        label: &str,
        primary: bool,
        disabled: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let bg = if primary {
            theme.accent
        } else {
            crate::theme::ink(0.04)
        };
        let fg = if primary { theme.on_accent } else { theme.text };
        div()
            .id(id)
            .h(px(30.0))
            .px(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if primary { theme.accent } else { theme.border })
            .bg(bg)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(fg)
            .when(disabled, |el| el.opacity(0.5))
            .when(!disabled, |el| {
                el.cursor_pointer().hover(move |s| s.opacity(0.9))
            })
            .child(SharedString::from(label.to_string()))
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let status = self.status.clone().unwrap_or_default();
        let have_status = self.status.is_some();
        let busy = self.busy;
        let can_enable = have_status && status.blocker.is_none() && !busy;

        let mut card = div()
            .w_full()
            .max_w(px(420.0))
            .p(px(16.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(crate::theme::ink(0.02))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        icon(icons::GLOBE)
                            .size(px(18.0))
                            .flex_none()
                            .text_color(theme.text),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(14.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(SharedString::from("Browser use")),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(
                                        "Headless Chrome via chrome-devtools-mcp: navigate, snapshot, click, fill, read console errors.",
                                    )),
                            ),
                    ),
            );

        if !have_status && self.error.is_none() {
            card = card.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Probing Chrome and npx…")),
            );
        } else {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(Self::probe_row(
                        &theme,
                        status.chrome_installed,
                        "Chrome / Chromium",
                        status.chrome_path.as_deref(),
                    ))
                    .child(Self::probe_row(
                        &theme,
                        status.npx_installed,
                        "npx (Node.js)",
                        Some(status.package.as_str()).filter(|p| !p.is_empty()),
                    ))
                    .child(Self::probe_row(
                        &theme,
                        status.enabled,
                        if status.enabled {
                            "MCP server registered"
                        } else {
                            "MCP server not registered"
                        },
                        status.enabled.then_some(if status.always_load {
                            "every compatible run"
                        } else {
                            "per-chat assignment"
                        }),
                    )),
            );
        }

        if let Some(blocker) = status.blocker.as_deref().filter(|_| !status.enabled) {
            card = card.child(
                div()
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.warning_muted)
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(SharedString::from(blocker.to_string())),
            );
        }
        if let Some(error) = self.error.clone() {
            card = card.child(
                div()
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.danger_muted)
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(error),
            );
        }

        let mut actions = div().flex().flex_row().items_center().gap(px(8.0));
        if status.enabled {
            actions = actions
                .child(
                    Self::button("browser-disable", &theme, "Disable", false, busy)
                        .when(!busy, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| this.disable(cx)))
                        }),
                )
                .child(
                    Self::button(
                        "browser-always-load",
                        &theme,
                        if status.always_load {
                            "Always on: yes"
                        } else {
                            "Always on: no"
                        },
                        false,
                        busy,
                    )
                    .when(!busy, |el| {
                        let next = !status.always_load;
                        el.on_click(
                            cx.listener(move |this, _, _, cx| this.set_always_load(next, cx)),
                        )
                    }),
                );
        } else {
            actions = actions.child(
                Self::button(
                    "browser-enable",
                    &theme,
                    "Enable browser",
                    true,
                    !can_enable,
                )
                .when(can_enable, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.enable(cx)))
                }),
            );
        }
        actions = actions.child(
            Self::button("browser-refresh", &theme, "Re-check", false, busy).when(!busy, |el| {
                el.on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
            }),
        );
        card = card.child(actions).child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(
                    "Once enabled, agents on Claude Code, OpenCode, Grok, Hermes, Pi and Cline get the chrome-devtools tools. Set CHROME_PATH to point at a custom Chrome.",
                )),
        );

        div()
            .size_full()
            .flex()
            .items_start()
            .justify_center()
            .p(px(16.0))
            .child(card)
    }
}
