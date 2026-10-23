//! Browser surface (issue #45): a real view on Komet's own headless Chrome
//! (`crates/engine/src/browser.rs`). Paste any link in the URL field and the
//! page renders here as a stream of screenshots; clicks, wheel and typing
//! are forwarded over CDP. Agents drive the SAME tab through
//! `chrome-devtools-mcp --browserUrl`, so an agent's browsing is visible
//! live — the integrated browser-use experience.
//!
//! No embedded webview (gpui): frames are JPEGs polled from the engine
//! while the surface is open.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable as _, Image, ImageFormat, KeyDownEvent,
    MouseButton, MouseDownEvent, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    Window, div, img, prelude::*, px,
};
use komet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const FRAME_INTERVAL: Duration = Duration::from_millis(500);

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

/// `BrowserFrame` / `BrowserNavigate` reply.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct FrameReply {
    running: Option<bool>,
    url: String,
    title: String,
    image_base64: Option<String>,
    width: u32,
    height: u32,
    can_go_back: bool,
    can_go_forward: bool,
}

#[derive(Clone)]
struct Frame {
    url: SharedString,
    title: SharedString,
    image: Option<Arc<Image>>,
    width: f32,
    height: f32,
    can_go_back: bool,
    can_go_forward: bool,
}

/// Where the last frame was painted — maps pointer events back to page
/// pixels.
#[derive(Clone, Copy, Default)]
struct Placement {
    origin: gpui::Point<gpui::Pixels>,
    size: gpui::Size<gpui::Pixels>,
}

pub struct BrowserPanel {
    state: Entity<AppState>,
    url_input: Entity<ComposerInput>,
    _url_events: Subscription,
    page_focus: FocusHandle,
    status: Option<BrowserUseStatus>,
    error: Option<SharedString>,
    loaded: bool,
    busy: bool,
    status_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    nav_task: Option<Task<()>>,
    frame: Option<Frame>,
    /// Navigation in flight — the URL field shows it until the frame lands.
    pending_url: Option<String>,
    placement: Rc<std::cell::Cell<Placement>>,
    /// Last frame bytes hash — skip re-decoding identical JPEGs.
    frame_fingerprint: u64,
    focus_pending: bool,
    /// The user has typed in the URL field since the last navigation.
    url_editing: bool,
    url_set_muted: bool,
}

impl BrowserPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let url_input =
            cx.new(|cx| ComposerInput::with_context("Search or enter URL", "PaletteSearch", cx));
        let url_events = cx.subscribe(&url_input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.submit_url(cx),
            // `set_text` also emits Edited; the mute distinguishes the user's
            // typing (which must not be clobbered by frame updates).
            ComposerInputEvent::Edited => {
                if !std::mem::take(&mut this.url_set_muted) {
                    this.url_editing = true;
                }
            }
            _ => {}
        });
        Self {
            state,
            url_input,
            _url_events: url_events,
            page_focus: cx.focus_handle(),
            status: None,
            error: None,
            loaded: false,
            busy: false,
            status_task: None,
            poll_task: None,
            nav_task: None,
            frame: None,
            pending_url: None,
            placement: Rc::new(std::cell::Cell::new(Placement::default())),
            frame_fingerprint: 0,
            focus_pending: true,
            url_editing: false,
            url_set_muted: false,
        }
    }

    pub fn tab_title(&self) -> SharedString {
        match self
            .frame
            .as_ref()
            .filter(|f| f.url.as_ref() != "about:blank")
        {
            Some(f) if !f.title.is_empty() => f.title.clone(),
            Some(f) => host_of(&f.url).into(),
            None => "about:blank".into(),
        }
    }

    /// First render (and every re-open): probe status and start polling
    /// frames so an agent's browsing shows up even before the user types.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        self.refresh_status(cx);
        self.start_polling(cx);
    }

    fn engine(&self, cx: &gpui::App) -> Option<crate::state::EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.status_call(methods::BROWSER_USE_STATUS, serde_json::json!({}), cx);
    }

    fn enable(&mut self, cx: &mut Context<Self>) {
        self.status_call(methods::ENABLE_BROWSER_USE, serde_json::json!({}), cx);
    }

    fn disable(&mut self, cx: &mut Context<Self>) {
        self.status_call(methods::DISABLE_BROWSER_USE, serde_json::json!({}), cx);
    }

    fn status_call(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            self.error = Some("Engine not connected.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        self.status_task = Some(cx.spawn(async move |this, cx| {
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

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll_task.is_some() {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let reply = engine
                    .client()
                    .call(methods::BROWSER_FRAME, serde_json::json!({}))
                    .await;
                let alive = this
                    .update(cx, |panel, cx| {
                        if let Ok(value) = reply {
                            panel.apply_frame(value, cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(FRAME_INTERVAL).await;
            }
        }));
    }

    fn apply_frame(&mut self, value: serde_json::Value, cx: &mut Context<Self>) {
        let Ok(reply) = serde_json::from_value::<FrameReply>(value) else {
            return;
        };
        if reply.running == Some(false) {
            return; // Chrome not started yet — keep the empty state.
        }
        let fingerprint = reply.image_base64.as_deref().map(fnv1a).unwrap_or(0);
        let image = if fingerprint == self.frame_fingerprint {
            self.frame.as_ref().and_then(|f| f.image.clone())
        } else {
            self.frame_fingerprint = fingerprint;
            reply.image_base64.as_deref().and_then(|b64| {
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .ok()
                    .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Jpeg, bytes)))
            })
        };
        let url_changed = self
            .frame
            .as_ref()
            .is_none_or(|f| f.url.as_ref() != reply.url);
        self.frame = Some(Frame {
            url: reply.url.clone().into(),
            title: reply.title.into(),
            image,
            width: reply.width.max(1) as f32,
            height: reply.height.max(1) as f32,
            can_go_back: reply.can_go_back,
            can_go_forward: reply.can_go_forward,
        });
        // Follow the page (agent navigations included) unless the user is
        // mid-edit in the URL field.
        if url_changed
            && self.pending_url.is_none()
            && reply.url != "about:blank"
            && !self.url_editing
        {
            self.set_url_text(reply.url.clone(), cx);
        }
        if self.pending_url.as_deref() == Some(reply.url.as_str()) {
            self.pending_url = None;
        }
        cx.notify();
    }

    fn browser_call(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            self.error = Some("Engine not connected.".into());
            cx.notify();
            return;
        };
        self.error = None;
        self.nav_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            let _ = this.update(cx, |panel, cx| match result {
                Ok(value) => {
                    if value.get("url").is_some() {
                        panel.apply_frame(value, cx);
                    }
                }
                Err(err) => {
                    panel.pending_url = None;
                    panel.error = Some(err.to_string().into());
                    cx.notify();
                }
            });
        }));
    }

    fn set_url_text(&mut self, url: String, cx: &mut Context<Self>) {
        self.url_set_muted = true;
        self.url_input
            .update(cx, |input, cx| input.set_text(url, cx));
        self.url_set_muted = false;
        self.url_editing = false;
    }

    /// Enter in the URL field: normalise and navigate Komet's Chrome.
    fn submit_url(&mut self, cx: &mut Context<Self>) {
        let raw = self.url_input.read(cx).text().trim().to_string();
        if raw.is_empty() {
            return;
        }
        let url = normalize_url(&raw);
        self.set_url_text(url.clone(), cx);
        self.pending_url = Some(url.clone());
        self.browser_call(
            methods::BROWSER_NAVIGATE,
            serde_json::json!({ "url": url }),
            cx,
        );
        self.start_polling(cx);
        cx.notify();
    }

    fn history(&mut self, delta: i64, cx: &mut Context<Self>) {
        self.browser_call(
            methods::BROWSER_HISTORY,
            serde_json::json!({ "delta": delta }),
            cx,
        );
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.refresh_status(cx);
        if self.frame.is_some() {
            self.browser_call(methods::BROWSER_RELOAD, serde_json::json!({}), cx);
        }
    }

    fn send_input(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        cx.spawn(async move |_, _| {
            let _ = engine.client().call(methods::BROWSER_INPUT, params).await;
        })
        .detach();
    }

    /// Window position → page pixels, if inside the painted frame.
    fn page_point(&self, position: gpui::Point<gpui::Pixels>) -> Option<(f64, f64)> {
        let frame = self.frame.as_ref()?;
        let place = self.placement.get();
        let w = f32::from(place.size.width);
        let h = f32::from(place.size.height);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let rx = (f32::from(position.x) - f32::from(place.origin.x)) / w;
        let ry = (f32::from(position.y) - f32::from(place.origin.y)) / h;
        if !(0.0..=1.0).contains(&rx) || !(0.0..=1.0).contains(&ry) {
            return None;
        }
        Some(((rx * frame.width) as f64, (ry * frame.height) as f64))
    }

    fn on_page_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.page_focus, cx);
        if let Some((x, y)) = self.page_point(event.position) {
            self.send_input(serde_json::json!({ "kind": "click", "x": x, "y": y }), cx);
        }
    }

    fn on_page_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((x, y)) = self.page_point(event.position) else {
            return;
        };
        let delta = match event.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * 40.0, l.y * 40.0),
        };
        // gpui's wheel delta points the way content moves; CDP wants the
        // opposite sign.
        self.send_input(
            serde_json::json!({
                "kind": "scroll", "x": x, "y": y,
                "dx": -(delta.0 as f64), "dy": -(delta.1 as f64)
            }),
            cx,
        );
    }

    fn on_page_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let params = match ks.key.as_str() {
            "enter" | "backspace" | "tab" | "escape" => {
                serde_json::json!({ "kind": "key", "key": ks.key })
            }
            _ => match ks.key_char.as_deref() {
                Some(text) if !ks.modifiers.control && !ks.modifiers.platform => {
                    serde_json::json!({ "kind": "text", "text": text })
                }
                _ => return,
            },
        };
        self.send_input(params, cx);
        cx.stop_propagation();
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
        let (back, forward) = self
            .frame
            .as_ref()
            .map(|f| (f.can_go_back, f.can_go_forward))
            .unwrap_or((false, false));
        let current = self
            .frame
            .as_ref()
            .filter(|f| f.url.as_ref() != "about:blank")
            .map(|f| f.url.to_string());
        let loading = self.pending_url.is_some();
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
                Self::toolbar_button(theme, "browser-back", icons::ALT_ARROW_LEFT, back)
                    .when(back, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| this.history(-1, cx)))
                    }),
            )
            .child(
                Self::toolbar_button(theme, "browser-forward", icons::ALT_ARROW_RIGHT, forward)
                    .when(forward, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| this.history(1, cx)))
                    }),
            )
            .child(
                Self::toolbar_button(theme, "browser-reload", icons::REFRESH, !loading)
                    .when(!loading, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
                    }),
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
                    .border_color(if loading { theme.accent } else { theme.border })
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

    /// Status pill under the toolbar: probe results and the agent toggle.
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
                        "Agents can browse"
                    } else {
                        "Agents can't browse"
                    })),
            )
            .child(toggle)
            .into_any_element()
    }

    fn render_page(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(frame) = self.frame.clone().filter(|f| f.image.is_some()) else {
            return self.render_empty(theme, cx);
        };
        let image = frame.image.clone().expect("filtered");
        let placement = self.placement.clone();
        let aspect = frame.width / frame.height;
        div()
            .id("browser-page")
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(gpui::white())
            .flex()
            .items_start()
            .justify_center()
            .overflow_hidden()
            .track_focus(&self.page_focus)
            .key_context("BrowserPage")
            .on_key_down(cx.listener(Self::on_page_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_page_mouse_down))
            .on_scroll_wheel(cx.listener(Self::on_page_scroll))
            .cursor_pointer()
            .child(
                div()
                    .w_full()
                    .relative()
                    .child(
                        img(image)
                            .w_full()
                            .h_auto()
                            .object_fit(gpui::ObjectFit::Contain),
                    )
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| {
                                // The image keeps the page aspect; its painted
                                // height follows the width.
                                let w = f32::from(bounds.size.width);
                                let h = w / aspect;
                                placement.set(Placement {
                                    origin: bounds.origin,
                                    size: gpui::size(px(w), px(h)),
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
            .into_any_element()
    }

    fn render_empty(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let status = self.status.clone().unwrap_or_default();
        let blocked = status.blocker.clone().filter(|_| !status.enabled);
        let chrome_ok = status.chrome_installed;
        let can_enable = self.status.is_some() && !self.busy && blocked.is_none();

        let (title, hint): (&str, &str) = match (self.pending_url.is_some(), chrome_ok) {
            (true, _) => ("Loading…", "Starting Chrome and opening the page."),
            (false, true) => (
                "Start browsing",
                "Paste a link above to open it in Komet's headless Chrome. Agents with browsing on drive this same tab, live.",
            ),
            (false, false) => (
                "Chrome required",
                "Install Google Chrome or Chromium (or set CHROME_PATH) to browse here and let agents browse.",
            ),
        };

        let mut body = div()
            .flex()
            .flex_col()
            .items_center()
            .max_w(px(380.0))
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
                    .child(SharedString::from(hint.to_string())),
            );
        if !status.enabled && chrome_ok {
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
                    .child(SharedString::from("Let agents browse")),
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
        let page = self.render_page(&theme, cx);
        let mut root = div()
            .id("browser-surface")
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(status);
        if let Some(error) = self.error.clone().filter(|_| self.frame.is_some()) {
            root = root.child(
                div()
                    .px(px(14.0))
                    .child(widgets::error_strip(&theme, error)),
            );
        }
        root.child(page)
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// `example.com/x` → `https://example.com/x`; bare words → a search URL;
/// `localhost:3000` keeps `http`.
pub fn normalize_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") || raw.starts_with("data:") || raw.starts_with("about:") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hosts_localhost_and_searches() {
        assert_eq!(normalize_url("example.com/a"), "https://example.com/a");
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("https://x.io"), "https://x.io");
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(
            normalize_url("rust gpui docs"),
            "https://duckduckgo.com/?q=rust+gpui+docs"
        );
        assert_eq!(host_of("https://example.com/a/b"), "example.com");
    }
}
