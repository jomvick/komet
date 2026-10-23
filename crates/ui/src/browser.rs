//! Browser surface (issue #45): a browser-shaped frame in the right pane
//! for the browser-use preset. Toolbar with a URL field, a "Start browsing"
//! empty state, and a compact status line for the Chrome / npx probe from
//! `BrowserUseStatus`. There is no embedded webview: entering a URL hands a
//! browse task to the agent, which drives headless Chrome through
//! `chrome-devtools-mcp`; its steps and screenshots land in the transcript.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, Focusable as _, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};
use komet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
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

/// Shell-facing events: the URL field submits a browse task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserEvent {
    /// Ask the current chat's agent to open `url` in headless Chrome.
    Browse { url: String, prompt: String },
}

pub struct BrowserPanel {
    state: Entity<AppState>,
    url_input: Entity<ComposerInput>,
    _url_events: Subscription,
    status: Option<BrowserUseStatus>,
    error: Option<SharedString>,
    loaded: bool,
    busy: bool,
    task: Option<Task<()>>,
    /// Last URL handed to the agent — the tab title and the content hint.
    current_url: Option<String>,
    history: Vec<String>,
    focus_pending: bool,
}

impl EventEmitter<BrowserEvent> for BrowserPanel {}

impl BrowserPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let url_input =
            cx.new(|cx| ComposerInput::with_context("Search or enter URL", "PaletteSearch", cx));
        let url_events = cx.subscribe(&url_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_url(cx);
            }
        });
        Self {
            state,
            url_input,
            _url_events: url_events,
            status: None,
            error: None,
            loaded: false,
            busy: false,
            task: None,
            current_url: None,
            history: Vec::new(),
            focus_pending: true,
        }
    }

    pub fn tab_title(&self) -> SharedString {
        match self.current_url.as_deref() {
            Some(url) => host_of(url).into(),
            None => "about:blank".into(),
        }
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

    /// Enter in the URL field: normalise, remember, and hand the agent a
    /// browse task. Plain words become a search like a real omnibox.
    fn submit_url(&mut self, cx: &mut Context<Self>) {
        let raw = self.url_input.read(cx).text().trim().to_string();
        if raw.is_empty() {
            return;
        }
        let url = normalize_url(&raw);
        self.url_input
            .update(cx, |input, cx| input.set_text(url.clone(), cx));
        if self.current_url.as_deref() != Some(url.as_str()) {
            if let Some(prev) = self.current_url.replace(url.clone()) {
                self.history.push(prev);
            }
        }
        let prompt = browse_prompt(&url);
        cx.emit(BrowserEvent::Browse { url, prompt });
        cx.notify();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(prev) = self.history.pop() {
            self.current_url = Some(prev.clone());
            self.url_input
                .update(cx, |input, cx| input.set_text(prev, cx));
            cx.notify();
        }
    }

    fn toolbar_button(
        theme: &Theme,
        id: &'static str,
        icon_path: &'static str,
        enabled: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .size(px(28.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(if enabled {
                theme.text_muted
            } else {
                theme.text_muted.opacity(0.35)
            })
            .when(enabled, |el| {
                el.cursor_pointer().hover(|s| s.bg(crate::theme::ink(0.06)))
            })
            .child(icon(icon_path).size(px(15.0)))
    }

    fn render_toolbar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let can_back = !self.history.is_empty();
        let busy = self.busy;
        let current = self.current_url.clone();
        div()
            .flex_none()
            .h(px(44.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(
                Self::toolbar_button(theme, "browser-back", icons::ALT_ARROW_LEFT, can_back)
                    .when(can_back, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| this.go_back(cx)))
                    }),
            )
            .child(Self::toolbar_button(
                theme,
                "browser-forward",
                icons::ALT_ARROW_RIGHT,
                false,
            ))
            .child(
                Self::toolbar_button(theme, "browser-reload", icons::REFRESH, !busy).when(
                    !busy,
                    |el| {
                        el.on_click(cx.listener(|this, _, _, cx| {
                            // Re-probe Chrome / npx and, with a page open,
                            // re-send the browse task.
                            this.refresh(cx);
                            if this.current_url.is_some() {
                                this.submit_url(cx);
                            }
                        }))
                    },
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .mx(px(4.0))
                    .h(px(32.0))
                    .px(px(12.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.04))
                    .flex()
                    .items_center()
                    .text_size(px(12.5))
                    .child(div().w_full().child(self.url_input.clone())),
            )
            .child(
                Self::toolbar_button(
                    theme,
                    "browser-external",
                    icons::ARROW_UP_RIGHT,
                    current.is_some(),
                )
                .when_some(current, |el, url| {
                    el.on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                }),
            )
    }

    /// Status pill under the toolbar: probe results and the enable toggle.
    fn render_status_line(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(status) = self.status.clone() else {
            return div()
                .px(px(14.0))
                .py(px(6.0))
                .text_size(px(11.5))
                .text_color(theme.text_muted.opacity(0.65))
                .child(SharedString::from(if self.error.is_some() {
                    "Status unavailable."
                } else {
                    "Probing Chrome and npx…"
                }))
                .into_any_element();
        };
        let pill = |ok: bool, label: String| -> AnyElement {
            if ok {
                widgets::badge_active(theme, label).into_any_element()
            } else {
                widgets::badge(theme, label)
                    .text_color(theme.danger_muted)
                    .into_any_element()
            }
        };
        let blocked = status.blocker.is_some() && !status.enabled;
        let busy = self.busy;
        let enabled = status.enabled;
        let toggle = widgets::toggle_switch(theme, enabled)
            .id("browser-enable")
            .when(busy || blocked, |el| el.opacity(0.5))
            .when(!busy && !blocked, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if enabled {
                            this.disable(cx)
                        } else {
                            this.enable(cx)
                        }
                    }))
            });
        div()
            .px(px(14.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .flex_wrap()
            .gap(px(8.0))
            .child(pill(
                status.chrome_installed,
                if status.chrome_installed {
                    "Chrome".into()
                } else {
                    "Chrome missing".into()
                },
            ))
            .child(pill(
                status.npx_installed,
                if status.npx_installed {
                    status.package.clone()
                } else {
                    "npx missing".into()
                },
            ))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(if enabled {
                        "Agent browsing on"
                    } else {
                        "Agent browsing off"
                    })),
            )
            .child(toggle)
            .into_any_element()
    }

    fn render_content(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let status = self.status.clone().unwrap_or_default();
        let blocked = status.blocker.clone().filter(|_| !status.enabled);
        let enabled = status.enabled;
        let can_enable = self.status.is_some() && !self.busy && blocked.is_none();

        let (title, hint): (&str, SharedString) = match self.current_url.as_deref() {
            Some(url) => (
                "Browsing",
                format!("{url}\nThe agent drives headless Chrome; its steps, console errors and screenshots land in the chat.")
                    .into(),
            ),
            None if enabled => (
                "Start browsing",
                "Enter a URL above — the agent opens it in headless Chrome and reports back."
                    .into(),
            ),
            None => (
                "Start browsing",
                "Turn on agent browsing to let the agent open pages, click, fill forms and read console errors."
                    .into(),
            ),
        };

        let mut body = div()
            .flex()
            .flex_col()
            .items_center()
            .max_w(px(360.0))
            .gap(px(12.0))
            .child(
                icon(icons::GLOBE)
                    .size(px(32.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(title.to_string())),
            )
            .child(
                div()
                    .text_center()
                    .text_size(px(12.5))
                    .line_height(px(18.0))
                    .text_color(theme.text_muted)
                    .child(hint),
            );
        if !enabled {
            body = body.child(
                div()
                    .id("browser-enable-cta")
                    .mt(px(4.0))
                    .px(px(14.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(theme.text)
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.on_solid)
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(!can_enable, |el| el.opacity(0.5))
                    .when(can_enable, |el| {
                        el.cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| this.enable(cx)))
                    })
                    .child(icon(icons::CHECK).size(px(14.0)))
                    .child(SharedString::from("Enable agent browsing")),
            );
        }
        if let Some(blocker) = blocked {
            body = body.child(widgets::warning_strip(theme, blocker));
        }
        if let Some(error) = self.error.clone() {
            body = body.child(widgets::error_strip(theme, error));
        }

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(body)
            .into_any_element()
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.url_input.focus_handle(cx), cx);
        }
        let toolbar = self.render_toolbar(&theme, cx);
        let status = self.render_status_line(&theme, cx);
        let content = self.render_content(&theme, cx);
        div()
            .id("browser-surface")
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(status)
            .child(content)
    }
}

/// `example.com/x` → `https://example.com/x`; bare words → a search URL;
/// `localhost:3000` keeps `http`.
pub fn normalize_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") {
        return raw.to_string();
    }
    let head = raw.split('/').next().unwrap_or(raw);
    let looks_like_host = head.starts_with("localhost")
        || head.starts_with("127.")
        || (head.contains('.') && !head.contains(' '));
    if looks_like_host {
        let scheme = if head.starts_with("localhost") || head.starts_with("127.") {
            "http"
        } else {
            "https"
        };
        return format!("{scheme}://{raw}");
    }
    format!(
        "https://duckduckgo.com/?q={}",
        raw.split_whitespace().collect::<Vec<_>>().join("+")
    )
}

pub fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// The browse task handed to the agent (the chrome-devtools MCP tools do the
/// rest). Kept short: the server's own descriptions carry the workflow.
pub fn browse_prompt(url: &str) -> String {
    format!(
        "Open {url} in the headless browser (chrome-devtools MCP: navigate_page, then take_snapshot \
         and take_screenshot). Describe what the page shows, list any console errors from \
         list_console_messages, and finish with a short JSON report \
         {{\"overallStatus\",\"summary\",\"finalUrl\",\"finalPageTitle\",\"results\",\"consoleErrors\",\"lessons\"}}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hosts_localhost_and_searches() {
        assert_eq!(normalize_url("example.com/a"), "https://example.com/a");
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("https://x.io"), "https://x.io");
        assert_eq!(
            normalize_url("rust gpui docs"),
            "https://duckduckgo.com/?q=rust+gpui+docs"
        );
        assert_eq!(host_of("https://example.com/a/b"), "example.com");
        assert!(browse_prompt("https://x.io").contains("navigate_page"));
    }
}
