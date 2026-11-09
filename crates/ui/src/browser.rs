mod cdp;
mod process;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gpui::{
    AnyElement, App, Bounds, Context, Element, Entity, FocusHandle, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton, ObjectFit, Pixels,
    Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Window, div, img, prelude::*,
    px,
};
use image::{Frame, RgbaImage};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect as ws_connect};

pub use cdp::{ConsoleLog, OverlayElement, cdp_modifiers};
use process::BrowserProcess;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;

const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);
type WsStream = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

pub enum BrowserCommand {
    Back,
    Forward,
    GetNavHistory,
    Reload,
    Retry,
    Navigate(String),
    KeyPress {
        key: &'static str,
        code: &'static str,
        vk: Option<u32>,
        modifiers: u32,
    },
    KeyRelease {
        key: &'static str,
        code: &'static str,
        modifiers: u32,
    },
    InsertText(String),
    MousePress {
        x: f32,
        y: f32,
        button: &'static str,
        count: u32,
    },
    MouseRelease {
        x: f32,
        y: f32,
        button: &'static str,
        count: u32,
    },
    MouseMove {
        x: f32,
        y: f32,
    },
    Scroll {
        x: f32,
        y: f32,
        delta_x: f64,
        delta_y: f64,
    },
    TypeText(String),
    Resize {
        width: u32,
        height: u32,
        scale: f32,
    },
    CaptureScreenshot,
    ToggleOverlay,
    Evaluate(String),
    Shutdown,
}

pub enum BrowserEvent {
    Page {
        image: Arc<RenderImage>,
        title: String,
        url: String,
    },
    Title(String),
    Loading(bool),
    NavHistory {
        can_back: bool,
        can_forward: bool,
    },
    Elements(Vec<OverlayElement>),
    Console(ConsoleLog),
    Screenshot(Vec<u8>),
    Failed(String),
}

enum SessionResult {
    Retry,
    Shutdown,
}

pub struct BrowserSurface {
    commands: Sender<BrowserCommand>,
    events: Receiver<BrowserEvent>,
    address: Entity<ComposerInput>,
    _address_subscription: gpui::Subscription,
    console_input: Entity<ComposerInput>,
    _console_subscription: gpui::Subscription,
    page_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    page_focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    retired_images: Vec<Arc<RenderImage>>,
    title: String,
    url: String,
    is_loading: bool,
    can_back: bool,
    can_forward: bool,
    error: Option<String>,
    elements: Vec<OverlayElement>,
    show_overlay: bool,
    show_console: bool,
    console_logs: Vec<ConsoleLog>,
    status_toast: Option<String>,
    last_viewport: Cell<(u32, u32)>,
}

impl BrowserSurface {
    pub fn new(url: String, profile_key: String, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| ComposerInput::new("Search or enter an address", cx));
        address.update(cx, |input, cx| input.set_text(url.clone(), cx));

        let console_input = cx.new(|cx| ComposerInput::new("Evaluate JavaScript...", cx));

        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(8);
        let initial_url = url.clone();
        let startup_events = event_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("komet-cdp-browser".into())
            .spawn(move || run_client(command_rx, event_tx, initial_url, profile_key))
        {
            let _ = startup_events.send(BrowserEvent::Failed(format!(
                "Could not start browser CDP client: {error}"
            )));
        }

        let address_subscription =
            cx.subscribe(&address, |this, _input, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    this.submit_address(cx);
                }
            });

        let console_subscription = cx.subscribe(
            &console_input,
            |this, input, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    let expr = input.read(cx).text().trim().to_owned();
                    if !expr.is_empty() {
                        let _ = this.commands.send(BrowserCommand::Evaluate(expr.clone()));
                        this.console_logs.push(ConsoleLog {
                            level: "input".into(),
                            text: format!("> {expr}"),
                        });
                        this.console_input
                            .update(cx, |input, cx| input.set_text("", cx));
                    }
                    cx.notify();
                }
            },
        );

        let browser = Self {
            commands: command_tx,
            events: event_rx,
            address,
            _address_subscription: address_subscription,
            console_input,
            _console_subscription: console_subscription,
            page_bounds: Rc::new(Cell::new(None)),
            page_focus: cx.focus_handle(),
            image: None,
            retired_images: Vec::new(),
            title: "Browser".into(),
            url,
            is_loading: false,
            can_back: false,
            can_forward: false,
            error: None,
            elements: Vec::new(),
            show_overlay: false,
            show_console: false,
            console_logs: Vec::new(),
            status_toast: None,
            last_viewport: Cell::new((0, 0)),
        };

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(8))
                    .await;
                if this.update(cx, |view, cx| view.receive_events(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        browser
    }

    pub fn submit_address(&mut self, cx: &mut Context<Self>) {
        let value = self.address.read(cx).text().trim().to_owned();
        match normalize_url(&value) {
            Ok(url) => {
                self.url = url.clone();
                self.is_loading = true;
                self.error = None;
                self.address
                    .update(cx, |input, cx| input.set_text(url.clone(), cx));
                let _ = self.commands.send(BrowserCommand::Navigate(url));
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    pub fn tab_title(&self) -> SharedString {
        if self.title.trim().is_empty() {
            SharedString::from("Browser")
        } else {
            self.title.clone().into()
        }
    }

    fn receive_events(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        loop {
            match self.events.try_recv() {
                Ok(BrowserEvent::Page { image, title, url }) => {
                    if let Some(previous) = self.image.replace(image) {
                        self.retired_images.push(previous);
                    }
                    if !title.is_empty() {
                        self.title = title;
                    }
                    self.is_loading = false;
                    let previous_url = self.url.clone();
                    self.url = url;
                    if self.address.read(cx).text() == previous_url {
                        let address = self.url.clone();
                        self.address
                            .update(cx, |input, cx| input.set_text(address, cx));
                    }
                    self.error = None;
                    changed = true;

                    if self.show_overlay {
                        let _ = self.commands.send(BrowserCommand::ToggleOverlay);
                    }
                }
                Ok(BrowserEvent::Title(title)) => {
                    if !title.trim().is_empty() {
                        self.title = title;
                        changed = true;
                    }
                }
                Ok(BrowserEvent::Loading(loading)) => {
                    self.is_loading = loading;
                    changed = true;
                }
                Ok(BrowserEvent::NavHistory {
                    can_back,
                    can_forward,
                }) => {
                    if self.can_back != can_back || self.can_forward != can_forward {
                        self.can_back = can_back;
                        self.can_forward = can_forward;
                        changed = true;
                    }
                }
                Ok(BrowserEvent::Elements(elements)) => {
                    self.elements = elements;
                    changed = true;
                }
                Ok(BrowserEvent::Console(log)) => {
                    self.console_logs.push(log);
                    if self.console_logs.len() > 120 {
                        self.console_logs.remove(0);
                    }
                    changed = true;
                }
                Ok(BrowserEvent::Screenshot(bytes)) => {
                    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let file_name = format!("screenshot-{ts}.png");
                    let path = dirs::download_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(&file_name);
                    if let Ok(()) = std::fs::write(&path, &bytes) {
                        self.status_toast = Some(format!("Saved: {}", path.display()));
                    } else {
                        self.status_toast =
                            Some(format!("Screenshot captured ({} bytes)", bytes.len()));
                    }
                    changed = true;
                }
                Ok(BrowserEvent::Failed(error)) => {
                    self.error = Some(error);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.error.is_none() {
                        self.error = Some("Browser engine connection terminated.".into());
                        changed = true;
                    }
                    break;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn on_page_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = cdp_modifiers(
            keystroke.modifiers.alt,
            keystroke.modifiers.control,
            keystroke.modifiers.shift,
            keystroke.modifiers.platform,
        );

        // coller : presse-papier → Input.insertText (IME + collage fiables)
        let ctrl_or_meta = keystroke.modifiers.control || keystroke.modifiers.platform;
        if ctrl_or_meta && keystroke.key == "v" && !keystroke.modifiers.shift {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                && !text.is_empty()
            {
                let _ = self.commands.send(BrowserCommand::InsertText(text));
                cx.stop_propagation();
            }
            return;
        }

        // copier / couper / tout sélectionner : forward avec modifiers pour Chrome
        if ctrl_or_meta
            && matches!(keystroke.key.as_str(), "a" | "c" | "x")
            && !keystroke.modifiers.shift
        {
            let (key, code, vk) = match keystroke.key.as_str() {
                "a" => ("a", "KeyA", Some(65u32)),
                "c" => ("c", "KeyC", Some(67u32)),
                _ => ("x", "KeyX", Some(88u32)),
            };
            let _ = self.commands.send(BrowserCommand::KeyPress {
                key,
                code,
                vk,
                modifiers,
            });
            let _ = self.commands.send(BrowserCommand::KeyRelease {
                key,
                code,
                modifiers,
            });
            cx.stop_propagation();
            return;
        }

        if let Some((key, code, vk)) = gpui_key_to_cdp(&keystroke.key) {
            let _ = self.commands.send(BrowserCommand::KeyPress {
                key,
                code,
                vk: Some(vk),
                modifiers,
            });
            cx.stop_propagation();
            return;
        }

        if let Some(text) = keystroke
            .key_char
            .as_deref()
            .filter(|text| !text.is_empty() && !text.chars().all(char::is_control))
        {
            let _ = self
                .commands
                .send(BrowserCommand::TypeText(text.to_owned()));
            cx.stop_propagation();
        }
    }

    fn on_page_key_up(
        &mut self,
        event: &gpui::KeyUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = cdp_modifiers(
            keystroke.modifiers.alt,
            keystroke.modifiers.control,
            keystroke.modifiers.shift,
            keystroke.modifiers.platform,
        );
        if let Some((key, code, _vk)) = gpui_key_to_cdp(&keystroke.key) {
            let _ = self.commands.send(BrowserCommand::KeyRelease {
                key,
                code,
                modifiers,
            });
            cx.stop_propagation();
        }
    }
}

impl Drop for BrowserSurface {
    fn drop(&mut self) {
        let _ = self.commands.send(BrowserCommand::Shutdown);
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired_images.drain(..) {
            gpui::ImageSource::Render(image).evict(Some(window), cx);
        }

        let theme = crate::theme::Theme::of(cx).clone();
        // Position de la souris vue par le panneau ; press et release sont des
        // commandes séparées (drag + sélection de texte), moves = hover/menus.
        let press_commands = self.commands.clone();
        let release_commands = self.commands.clone();
        let move_commands = self.commands.clone();
        let scroll_commands = self.commands.clone();
        let press_bounds = self.page_bounds.clone();
        let release_bounds = self.page_bounds.clone();
        let move_bounds = self.page_bounds.clone();
        let scroll_bounds = self.page_bounds.clone();
        let page_focus = self.page_focus.clone();
        let image = self.image.clone();

        let page_view = image
            .map(|image| {
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            })
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_muted)
                    .text_size(px(13.0))
                    .child(if self.is_loading {
                        "Chargement…"
                    } else {
                        "Entrez une adresse pour commencer"
                    })
                    .into_any_element()
            });

        // Overlay element badges (inspiration Obscura)
        let overlay_elements = if self.show_overlay && !self.elements.is_empty() {
            let elements = self.elements.clone();
            div()
                .absolute()
                .inset_0()
                .children(elements.into_iter().map(|el| {
                    let label = format!("#{} {}", el.uid, el.text);
                    div()
                        .absolute()
                        .left(px(el.x))
                        .top(px(el.y))
                        .px(px(4.0))
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .bg(gpui::hsla(215.0 / 360.0, 0.9, 0.5, 0.85))
                        .text_color(gpui::white())
                        .text_size(px(10.0))
                        .cursor_pointer()
                        .child(label)
                }))
                .into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };

        let page = BrowserViewport {
            child: div()
                .id("komet-browser-page")
                .size_full()
                .relative()
                .track_focus(&self.page_focus)
                .on_key_down(cx.listener(Self::on_page_key_down))
                .on_key_up(cx.listener(Self::on_page_key_up))
                .on_mouse_down(MouseButton::Left, {
                    let page_focus = page_focus.clone();
                    let press_bounds = press_bounds.clone();
                    let press_commands = press_commands.clone();
                    move |event, window, cx| {
                        window.focus(&page_focus, cx);
                        if let Some((x, y)) = localize_in(&press_bounds, &event.position) {
                            let _ = press_commands.send(BrowserCommand::MousePress {
                                x,
                                y,
                                button: "left",
                                count: event.click_count as u32,
                            });
                        }
                    }
                })
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    window.focus(&page_focus, cx);
                    if let Some((x, y)) = localize_in(&press_bounds, &event.position) {
                        let _ = press_commands.send(BrowserCommand::MousePress {
                            x,
                            y,
                            button: "right",
                            count: event.click_count as u32,
                        });
                    }
                })
                .on_mouse_up(MouseButton::Left, {
                    let release_bounds = release_bounds.clone();
                    let release_commands = release_commands.clone();
                    move |event, _window, _cx| {
                        if let Some((x, y)) = localize_in(&release_bounds, &event.position) {
                            let _ = release_commands.send(BrowserCommand::MouseRelease {
                                x,
                                y,
                                button: "left",
                                count: event.click_count as u32,
                            });
                        }
                    }
                })
                .on_mouse_up(MouseButton::Right, move |event, _window, _cx| {
                    if let Some((x, y)) = localize_in(&release_bounds, &event.position) {
                        let _ = release_commands.send(BrowserCommand::MouseRelease {
                            x,
                            y,
                            button: "right",
                            count: event.click_count as u32,
                        });
                    }
                })
                .on_mouse_move(move |event, _window, cx| {
                    if let Some((x, y)) = localize_in(&move_bounds, &event.position) {
                        let _ = move_commands.send(BrowserCommand::MouseMove { x, y });
                        cx.stop_propagation();
                    }
                })
                .on_scroll_wheel(move |event: &ScrollWheelEvent, _, cx| {
                    let (delta_x, delta_y) = match event.delta {
                        ScrollDelta::Pixels(delta) => (f64::from(delta.x), f64::from(delta.y)),
                        ScrollDelta::Lines(delta) => {
                            (f64::from(delta.x) * 40.0, f64::from(delta.y) * 40.0)
                        }
                    };
                    // Use cursor position relative to view if available
                    let pos = scroll_bounds
                        .get()
                        .and_then(|b| b.localize(&event.position));
                    let (cx_f, cy_f) = pos
                        .map(|p| (f32::from(p.x), f32::from(p.y)))
                        .unwrap_or((320.0_f32, 240.0_f32));
                    let _ = scroll_commands.send(BrowserCommand::Scroll {
                        x: cx_f,
                        y: cy_f,
                        delta_x,
                        delta_y,
                    });
                    cx.stop_propagation();
                })
                .child(page_view)
                .child(overlay_elements)
                .into_any_element(),
            bounds: self.page_bounds.clone(),
        };

        // Check if viewport size changed and notify backend
        if let Some(bounds) = self.page_bounds.get() {
            let w = f32::from(bounds.size.width) as u32;
            let h = f32::from(bounds.size.height) as u32;
            if w > 100 && h > 100 {
                let prev = self.last_viewport.get();
                // Trigger on first valid bounds or when size changes by more than 8px
                if prev == (0, 0) || w.abs_diff(prev.0) > 8 || h.abs_diff(prev.1) > 8 {
                    self.last_viewport.set((w, h));
                    let scale = window.scale_factor();
                    let _ = self.commands.send(BrowserCommand::Resize {
                        width: w,
                        height: h,
                        scale,
                    });
                }
            }
        }

        let back = self.commands.clone();
        let forward = self.commands.clone();
        let reload = self.commands.clone();
        let capture = self.commands.clone();
        let retry = self.error.is_some();
        let can_back = self.can_back && !retry;
        let can_forward = self.can_forward && !retry;

        let overlay_active = self.show_overlay;
        let console_active = self.show_console;

        let entity_overlay = cx.entity().clone();
        let entity_console = cx.entity().clone();
        let entity_clear = cx.entity().clone();
        let entity_submit = cx.entity().clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(
                div()
                    .h(px(42.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(browser_button(
                        icons::ARROW_LEFT,
                        theme.text_muted,
                        can_back,
                        move || {
                            let _ = back.send(BrowserCommand::Back);
                        },
                    ))
                    .child(browser_button(
                        icons::ARROW_RIGHT,
                        theme.text_muted,
                        can_forward,
                        move || {
                            let _ = forward.send(BrowserCommand::Forward);
                        },
                    ))
                    .child(browser_button(
                        icons::REFRESH,
                        theme.text_muted,
                        true,
                        move || {
                            let _ = reload.send(if retry {
                                BrowserCommand::Retry
                            } else {
                                BrowserCommand::Reload
                            });
                        },
                    ))
                    .child(
                        div()
                            .h(px(28.0))
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .px(px(6.0))
                            .child(div().flex_1().min_w_0().child(self.address.clone()))
                            .child(
                                div()
                                    .id("navigate-go")
                                    .size(px(20.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(crate::theme::ink(0.08)))
                                    .on_click(move |_event, _window, cx| {
                                        entity_submit.update(cx, |this, cx| {
                                            this.submit_address(cx);
                                        });
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::ARROW_RIGHT)
                                            .size(px(12.0))
                                            .text_color(theme.text_muted),
                                    ),
                            ),
                    )
                    // Button: Capture / Screenshot
                    .child(browser_button(
                        icons::IMAGE,
                        theme.text_muted,
                        true,
                        move || {
                            let _ = capture.send(BrowserCommand::CaptureScreenshot);
                        },
                    ))
                    // Button: Toggle Obscura Interactive Elements Overlay
                    .child(browser_button_toggle(
                        icons::EYE,
                        overlay_active,
                        theme.text_muted,
                        move |_event, _window, cx| {
                            entity_overlay.update(cx, |this, cx| {
                                this.show_overlay = !this.show_overlay;
                                if this.show_overlay {
                                    let _ = this.commands.send(BrowserCommand::ToggleOverlay);
                                }
                                cx.notify();
                            });
                        },
                    ))
                    // Button: Toggle DevTools Console
                    .child(browser_button_toggle(
                        icons::TERMINAL,
                        console_active,
                        theme.text_muted,
                        move |_event, _window, cx| {
                            entity_console.update(cx, |this, cx| {
                                this.show_console = !this.show_console;
                                cx.notify();
                            });
                        },
                    )),
            )
            // Visual loading bar when navigation is in flight
            .when(self.is_loading, |el| {
                el.child(
                    div()
                        .h(px(3.0))
                        .flex_none()
                        .w_full()
                        .relative()
                        .bg(theme.accent.opacity(0.2))
                        .child(
                            // indeterminate stripe occupying ~40% of the bar
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .h_full()
                                .w(gpui::relative(0.4))
                                .bg(theme.accent),
                        ),
                )
            })
            // Status toast / screenshot notification
            .when_some(self.status_toast.clone(), |el, status| {
                el.child(
                    div()
                        .flex_none()
                        .bg(theme.surface)
                        .border_b_1()
                        .border_color(theme.border)
                        .px(px(10.0))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.accent)
                        .child(status),
                )
            })
            // Main page viewport
            .child(div().flex_1().min_h_0().w_full().child(page))
            // Console drawer
            .when(self.show_console, |el| {
                let logs = self.console_logs.clone();
                el.child(
                    div()
                        .h(px(160.0))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .child(
                            div()
                                .h(px(24.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(div().child("Console & DevTools"))
                                .child(
                                    div()
                                        .id("clear-console-logs")
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(theme.text))
                                        .on_click(move |_event, _window, cx| {
                                            entity_clear.update(cx, |this, cx| {
                                                this.console_logs.clear();
                                                cx.notify();
                                            });
                                        })
                                        .child("Clear"),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .p(px(6.0))
                                .text_size(px(11.0))
                                .children(logs.into_iter().map(|log| {
                                    let color: gpui::Hsla = match log.level.as_str() {
                                        "error" => gpui::rgb(0xff6b6b).into(),
                                        "warning" | "warn" => gpui::rgb(0xffd93d).into(),
                                        "input" => theme.accent,
                                        _ => theme.text,
                                    };
                                    div()
                                        .py(px(1.0))
                                        .text_color(color)
                                        .child(format!("[{}] {}", log.level, log.text))
                                })),
                        )
                        .child(
                            div()
                                .h(px(26.0))
                                .flex_none()
                                .border_t_1()
                                .border_color(theme.border)
                                .px(px(6.0))
                                .child(self.console_input.clone()),
                        ),
                )
            })
            // Error banner
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex_none()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(error),
                )
            })
    }
}

struct BrowserViewport {
    child: AnyElement,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl IntoElement for BrowserViewport {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BrowserViewport {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.bounds.set(Some(bounds));
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// Localise une position souris dans le panneau → (x, y) en pixels page.
fn localize_in(
    page_bounds: &Rc<Cell<Option<Bounds<Pixels>>>>,
    position: &gpui::Point<Pixels>,
) -> Option<(f32, f32)> {
    let bounds = page_bounds.get()?;
    let localized = bounds.localize(position)?;
    if f32::from(bounds.size.width) <= 0.0 || f32::from(bounds.size.height) <= 0.0 {
        return None;
    }
    Some((f32::from(localized.x), f32::from(localized.y)))
}

fn browser_button(
    icon_path: &'static str,
    color: gpui::Hsla,
    enabled: bool,
    action: impl Fn() + 'static,
) -> impl IntoElement {
    let color = if enabled {
        color
    } else {
        gpui::hsla(0.0, 0.0, 0.6, 0.35)
    };
    let mut button = div()
        .id(icon_path)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0));
    if enabled {
        button = button
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.06)))
            .on_click(move |_, _, _| action());
    }
    button.child(
        crate::icons::icon(icon_path)
            .size(px(14.0))
            .text_color(color),
    )
}

fn browser_button_toggle(
    icon_path: &'static str,
    active: bool,
    color: gpui::Hsla,
    action: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let bg = if active {
        crate::theme::ink(0.12)
    } else {
        gpui::transparent_black()
    };
    div()
        .id(icon_path)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(bg)
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.08)))
        .on_click(action)
        .child(
            crate::icons::icon(icon_path)
                .size(px(14.0))
                .text_color(color),
        )
}

fn gpui_render_image(image: RgbaImage) -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![Frame::new(image)]))
}

pub fn normalize_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Enter a URL or search term.".into());
    }
    if value.chars().any(char::is_control) {
        return Err("The address contains control characters.".into());
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("javascript:") {
        return Err("javascript: addresses are not supported.".into());
    }
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("about:")
    {
        return Ok(value.to_owned());
    }
    if value.contains("://") {
        return Err("Only HTTP and HTTPS addresses are supported.".into());
    }
    if is_local_target(value) {
        let expanded = value
            .strip_prefix(':')
            .map(|port| format!("localhost:{port}"))
            .unwrap_or_else(|| value.to_owned());
        return Ok(format!("http://{expanded}"));
    }
    if !value.chars().any(char::is_whitespace) && value.contains('.') {
        return Ok(format!("https://{value}"));
    }
    Ok(search_url(value))
}

fn percent_encode_query(query: &str) -> String {
    query
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>()
}

fn search_url(query: &str) -> String {
    let template = std::env::var("KOMET_SEARCH_URL")
        .unwrap_or_else(|_| "https://duckduckgo.com/?q={q}".into());
    template.replace("{q}", &percent_encode_query(query))
}

/// Souche sans schéma : localhost, *.localhost, plages privées, ou « :PORT » seul.
fn is_local_target(value: &str) -> bool {
    if let Some(port) = value.strip_prefix(':') {
        // saisie nue « :3000 » → localhost
        return !port.is_empty() && port.chars().all(|c| c.is_ascii_digit());
    }
    let rest = value.split_once("://").map(|(_, r)| r).unwrap_or(value);
    let host_part = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let bare = if host_part.starts_with('[') {
        host_part
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host_part)
    } else if host_part.matches(':').count() > 1 {
        // littéral IPv6 sans port (ex. ::1) — un port exigerait des crochets
        host_part
    } else {
        host_part.split(':').next().unwrap_or(host_part)
    };
    let host = bare.to_ascii_lowercase();
    host == "localhost"
        || host == "0.0.0.0"
        || host == "::1"
        || host.ends_with(".localhost")
        || private_ipv4(&host)
}

fn private_ipv4(host: &str) -> bool {
    let Ok(ip) = host.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let [a, b, ..] = ip.octets();
    a == 0 || a == 10 || a == 127 || (a == 192 && b == 168) || (a == 172 && (16..=31).contains(&b))
}

/// Suit quelle frame CDP est la frame principale, pour ne jamais confondre URL
/// d'iframe et URL de la barre d'adresse.
#[derive(Default)]
struct UrlTracker {
    main_frame_id: Option<String>,
}

impl UrlTracker {
    #[cfg(test)]
    fn main_frame_id(&self) -> Option<&str> {
        self.main_frame_id.as_deref()
    }

    /// Renvoie la nouvelle URL courante si l'événement affecte la frame principale.
    /// `Page.navigatedWithinDocument` porte l'URL dans `params.url` (pas
    /// `params.frame.url`) — c'était le bug de la barre d'URL sur les SPA.
    fn on_nav_event(&mut self, method: &str, params: &Value) -> Option<String> {
        if method == "Page.navigatedWithinDocument" {
            let frame_id = params.get("frameId").and_then(Value::as_str)?;
            if self.main_frame_id.as_deref() != Some(frame_id) {
                return None;
            }
            return params.get("url").and_then(Value::as_str).map(str::to_owned);
        }
        if method == "Page.frameNavigated" {
            let frame = params.get("frame")?;
            if frame.get("parentId").is_some() {
                // event frameNavigated est aussi émis pour les iframes
                return None;
            }
            let url = frame.get("url").and_then(Value::as_str)?.to_owned();
            if let Some(id) = frame.get("id").and_then(Value::as_str) {
                self.main_frame_id = Some(id.to_owned());
            }
            return Some(url);
        }
        None
    }

    fn on_loading_event(&self, method: &str, frame_id: &str) -> Option<bool> {
        if self.main_frame_id.as_deref() != Some(frame_id) {
            return None;
        }
        match method {
            "Page.frameStartedLoading" => Some(true),
            "Page.frameStoppedLoading" => Some(false),
            _ => None,
        }
    }
}

/// GPUI key name → (key, code, windowsVirtualKeyCode) CDP.
/// Les caractères imprimables ne passent pas ici (Input.char le fait déjà).
fn gpui_key_to_cdp(key: &str) -> Option<(&'static str, &'static str, u32)> {
    match key {
        "backspace" => Some(("Backspace", "Backspace", 8)),
        "enter" => Some(("Enter", "Enter", 13)),
        "tab" => Some(("Tab", "Tab", 9)),
        "escape" => Some(("Escape", "Escape", 27)),
        "delete" => Some(("Delete", "Delete", 46)),
        "left" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "right" => Some(("ArrowRight", "ArrowRight", 39)),
        "up" => Some(("ArrowUp", "ArrowUp", 38)),
        "down" => Some(("ArrowDown", "ArrowDown", 40)),
        "home" => Some(("Home", "Home", 36)),
        "end" => Some(("End", "End", 35)),
        _ => None,
    }
}

fn navigate_error_message(error_text: &str, url: &str) -> String {
    if error_text.contains("ERR_CONNECTION_REFUSED") {
        format!("{url} ne répond pas — lance ton serveur, puis clique Recharger/Réessayer.")
    } else if error_text.contains("ERR_NAME_NOT_RESOLVED") {
        format!("{url} : adresse introuvable ({error_text}).")
    } else {
        format!("{url} : {error_text}")
    }
}

fn run_client(
    commands: Receiver<BrowserCommand>,
    events: mpsc::SyncSender<BrowserEvent>,
    initial_url: String,
    profile_key: String,
) {
    let mut current_url = initial_url;
    loop {
        match run_session(&commands, &events, &mut current_url, &profile_key) {
            Ok(SessionResult::Shutdown) => break,
            Ok(SessionResult::Retry) => {}
            Err(error) => {
                if events.send(BrowserEvent::Failed(error)).is_err() {
                    break;
                }
                loop {
                    match commands.recv_timeout(Duration::from_secs(30)) {
                        Ok(BrowserCommand::Retry) => break,
                        Ok(BrowserCommand::Navigate(url)) => current_url = url,
                        Ok(BrowserCommand::Shutdown)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
    }
}

fn run_session(
    commands: &Receiver<BrowserCommand>,
    events: &mpsc::SyncSender<BrowserEvent>,
    current_url: &mut String,
    profile_key: &str,
) -> Result<SessionResult, String> {
    let (_process, page_ws_url) = BrowserProcess::spawn(profile_key, current_url)?;
    let (mut socket, _response) = ws_connect(&page_ws_url).map_err(|e| e.to_string())?;

    let mut next_id: u64 = 1;

    // Initialize CDP session
    socket
        .send(Message::Text(cdp::page_enable(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    socket
        .send(Message::Text(cdp::runtime_enable(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    // target=_blank / window.open : attacher automatiquement les popups pour
    // pouvoir les rediriger vers la vue courante (voir Target.attachedToTarget)
    socket
        .send(Message::Text(cdp::set_auto_attach(next_id)))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    // Set initial device metrics to match default viewport
    socket
        .send(Message::Text(cdp::set_device_metrics(
            next_id,
            DEFAULT_VIEWPORT.0,
            DEFAULT_VIEWPORT.1,
            1.0,
        )))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    socket
        .send(Message::Text(cdp::start_screencast(
            next_id,
            DEFAULT_VIEWPORT.0,
            DEFAULT_VIEWPORT.1,
            "jpeg",
            90,
        )))
        .map_err(|e| e.to_string())?;
    next_id += 1;

    socket
        .send(Message::Text(cdp::page_navigate(next_id, current_url)))
        .map_err(|e| e.to_string())?;
    let mut navigate_request_ids = std::collections::HashSet::new();
    navigate_request_ids.insert(next_id);
    next_id += 1;

    let mut page_title = String::new();
    let mut screenshot_request_id: Option<u64> = None;
    let mut title_request_id: Option<u64> = None;
    let mut overlay_request_id: Option<u64> = None;
    let mut dialog_request_id: Option<u64> = None;
    let mut nav_history_request_id: Option<u64> = None;
    let mut nav_history_step: i32 = 0;
    let mut eval_request_ids = std::collections::HashSet::new();
    let mut url_tracker = UrlTracker::default();

    // Set non-blocking on the underlying TCP stream so we can multiplex commands and frames
    set_socket_nonblocking(&mut socket, true)?;

    loop {
        // 1. Process pending UI commands
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                BrowserCommand::Back | BrowserCommand::Forward | BrowserCommand::GetNavHistory => {
                    let step = match cmd {
                        BrowserCommand::Back => -1,
                        BrowserCommand::Forward => 1,
                        _ => 0,
                    };
                    nav_history_request_id = Some(next_id);
                    nav_history_step = step;
                    socket
                        .send(Message::Text(cdp::get_navigation_history(next_id)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Reload => {
                    let _ = events.try_send(BrowserEvent::Loading(true));
                    socket
                        .send(Message::Text(cdp::page_reload(next_id)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Retry => return Ok(SessionResult::Retry),
                BrowserCommand::Navigate(url) => {
                    let _ = events.try_send(BrowserEvent::Loading(true));
                    socket
                        .send(Message::Text(cdp::page_navigate(next_id, &url)))
                        .map_err(|e| e.to_string())?;
                    navigate_request_ids.insert(next_id);
                    next_id += 1;
                    *current_url = url;
                }
                BrowserCommand::KeyPress {
                    key,
                    code,
                    vk,
                    modifiers,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_key_event(
                            next_id,
                            "rawKeyDown",
                            key,
                            code,
                            vk,
                            modifiers,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::KeyRelease {
                    key,
                    code,
                    modifiers,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_key_event(
                            next_id, "keyUp", key, code, None, modifiers,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::InsertText(text) => {
                    socket
                        .send(Message::Text(cdp::insert_text(next_id, &text)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::MousePress {
                    x,
                    y,
                    button,
                    count,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_click(
                            next_id,
                            "mousePressed",
                            x,
                            y,
                            button,
                            count,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::MouseRelease {
                    x,
                    y,
                    button,
                    count,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_click(
                            next_id,
                            "mouseReleased",
                            x,
                            y,
                            button,
                            count,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::MouseMove { x, y } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_move(next_id, x, y)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Scroll {
                    x,
                    y,
                    delta_x,
                    delta_y,
                } => {
                    socket
                        .send(Message::Text(cdp::dispatch_mouse_wheel(
                            next_id, x, y, delta_x, delta_y,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::TypeText(text) => {
                    for ch in text.chars() {
                        socket
                            .send(Message::Text(cdp::dispatch_char_event(
                                next_id,
                                &ch.to_string(),
                            )))
                            .map_err(|e| e.to_string())?;
                        next_id += 1;
                    }
                }
                BrowserCommand::Resize {
                    width,
                    height,
                    scale,
                } => {
                    socket
                        .send(Message::Text(cdp::set_device_metrics(
                            next_id, width, height, scale,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;

                    // Stop screencast before restarting with new dimensions
                    let _ = socket.send(Message::Text(cdp::stop_screencast(next_id)));
                    next_id += 1;

                    let physical_w = ((width as f32) * scale).round() as u32;
                    let physical_h = ((height as f32) * scale).round() as u32;
                    socket
                        .send(Message::Text(cdp::start_screencast(
                            next_id,
                            physical_w.max(width),
                            physical_h.max(height),
                            "jpeg",
                            90,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::CaptureScreenshot => {
                    screenshot_request_id = Some(next_id);
                    socket
                        .send(Message::Text(cdp::capture_screenshot(next_id)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::ToggleOverlay => {
                    overlay_request_id = Some(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(
                            next_id,
                            cdp::EXTRACT_ELEMENTS_SCRIPT,
                        )))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Evaluate(expr) => {
                    eval_request_ids.insert(next_id);
                    socket
                        .send(Message::Text(cdp::evaluate(next_id, &expr)))
                        .map_err(|e| e.to_string())?;
                    next_id += 1;
                }
                BrowserCommand::Shutdown => return Ok(SessionResult::Shutdown),
            }
        }

        // 2. Read CDP messages
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(val) = serde_json::from_str::<Value>(text.as_ref()) else {
                    continue;
                };

                // Check method
                if let Some(method) = val.get("method").and_then(Value::as_str) {
                    match method {
                        "Page.screencastFrame" => {
                            if let Some(params) = val.get("params") {
                                if let Some(session_id) =
                                    params.get("sessionId").and_then(Value::as_u64)
                                {
                                    let _ = socket.send(Message::Text(cdp::screencast_frame_ack(
                                        next_id, session_id,
                                    )));
                                    next_id += 1;
                                }

                                if let Some(base64_data) =
                                    params.get("data").and_then(Value::as_str)
                                    && let Ok(bytes) = STANDARD.decode(base64_data)
                                    && let Ok(dyn_image) = image::load_from_memory_with_format(
                                        &bytes,
                                        image::ImageFormat::Jpeg,
                                    )
                                {
                                    let rgba = dyn_image.into_rgba8();
                                    let render_img = gpui_render_image(rgba);
                                    let _ = events.try_send(BrowserEvent::Page {
                                        image: render_img,
                                        title: page_title.clone(),
                                        url: current_url.clone(),
                                    });
                                }
                            }
                        }
                        "Page.navigatedWithinDocument" | "Page.frameNavigated" => {
                            // Le UrlTracker filtre les iframes ; l'URL vient de
                            // params.url (navigatedWithinDocument) ou params.frame.url.
                            if let Some(params) = val.get("params")
                                && let Some(url) = url_tracker.on_nav_event(method, params)
                            {
                                *current_url = url;
                                // rafraîchit les états back/forward après chaque navigation
                                nav_history_request_id = Some(next_id);
                                nav_history_step = 0;
                                socket
                                    .send(Message::Text(cdp::get_navigation_history(next_id)))
                                    .map_err(|e| e.to_string())?;
                                next_id += 1;
                            }
                        }
                        "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
                            if let Some(frame_id) = val
                                .get("params")
                                .and_then(|p| p.get("frameId"))
                                .and_then(Value::as_str)
                                && let Some(loading) =
                                    url_tracker.on_loading_event(method, frame_id)
                            {
                                let _ = events.try_send(BrowserEvent::Loading(loading));
                                if method == "Page.frameStoppedLoading" {
                                    title_request_id = Some(next_id);
                                    let _ = socket.send(Message::Text(cdp::evaluate(
                                        next_id,
                                        "document.title",
                                    )));
                                    next_id += 1;
                                }
                            }
                        }
                        "Page.javascriptDialogOpening" => {
                            if let Some(params) = val.get("params") {
                                let message =
                                    params.get("message").and_then(Value::as_str).unwrap_or("");
                                let prompt = params
                                    .get("defaultPrompt")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                // auto-accept : sinon la page reste bloquée sur alert()
                                dialog_request_id = Some(next_id);
                                socket
                                    .send(Message::Text(cdp::handle_java_script_dialog(
                                        next_id,
                                        true,
                                        prompt.as_deref(),
                                    )))
                                    .map_err(|e| e.to_string())?;
                                next_id += 1;
                                let _ = events.send(BrowserEvent::Console(ConsoleLog {
                                    level: "dialog".into(),
                                    text: format!("{message} (accepté automatiquement)"),
                                }));
                            }
                        }
                        "Target.attachedToTarget" => {
                            if let Some(target_info) = val.pointer("/params/targetInfo") {
                                let is_page =
                                    target_info.get("type").and_then(Value::as_str) == Some("page");
                                let has_opener = target_info.get("openerId").is_some();
                                // popup ouvert par la page → on y navigue dans la vue courante
                                if is_page
                                    && has_opener
                                    && let Some(url) =
                                        target_info.get("url").and_then(Value::as_str)
                                {
                                    *current_url = url.to_owned();
                                    let _ = events.try_send(BrowserEvent::Loading(true));
                                    socket
                                        .send(Message::Text(cdp::page_navigate(next_id, url)))
                                        .map_err(|e| e.to_string())?;
                                    navigate_request_ids.insert(next_id);
                                    next_id += 1;
                                }
                            }
                        }
                        "Runtime.consoleAPICalled" => {
                            if let Some(params) = val.get("params") {
                                let level = params
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("log")
                                    .to_owned();
                                let mut text_parts = Vec::new();
                                if let Some(args) = params.get("args").and_then(Value::as_array) {
                                    for arg in args {
                                        if let Some(val) = arg.get("value") {
                                            text_parts.push(val.to_string());
                                        } else if let Some(desc) =
                                            arg.get("description").and_then(Value::as_str)
                                        {
                                            text_parts.push(desc.to_owned());
                                        }
                                    }
                                }
                                let _ = events.send(BrowserEvent::Console(ConsoleLog {
                                    level,
                                    text: text_parts.join(" "),
                                }));
                            }
                        }
                        "Runtime.exceptionThrown" => {
                            if let Some(params) = val.get("params") {
                                let desc = params
                                    .get("exceptionDetails")
                                    .and_then(|e| e.get("text"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("Uncaught exception")
                                    .to_owned();
                                let _ = events.send(BrowserEvent::Console(ConsoleLog {
                                    level: "error".into(),
                                    text: desc,
                                }));
                            }
                        }
                        _ => {}
                    }
                }

                // Check responses to requests
                if let Some(resp_id) = val.get("id").and_then(Value::as_u64) {
                    if navigate_request_ids.remove(&resp_id) {
                        if let Some(error_text) = val
                            .get("result")
                            .and_then(|r| r.get("errorText"))
                            .and_then(Value::as_str)
                            && !error_text.is_empty()
                        {
                            let _ = events.send(BrowserEvent::Failed(navigate_error_message(
                                error_text,
                                current_url,
                            )));
                        }
                    } else if dialog_request_id == Some(resp_id) {
                        dialog_request_id = None;
                    } else if nav_history_request_id == Some(resp_id) {
                        nav_history_request_id = None;
                        if let Some(result) = val.get("result") {
                            let index = result
                                .get("currentIndex")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                            let entries = result.get("entries").and_then(Value::as_array);
                            if let Some(entries) = entries {
                                let can_back = index > 0;
                                let can_forward = (index as usize) + 1 < entries.len();
                                let _ = events.try_send(BrowserEvent::NavHistory {
                                    can_back,
                                    can_forward,
                                });
                                if nav_history_step != 0 {
                                    let target = index + nav_history_step as i64;
                                    if let Some(entry) = entries.get(target.unsigned_abs() as usize)
                                        && let Some(entry_id) =
                                            entry.get("id").and_then(Value::as_u64)
                                    {
                                        socket
                                            .send(Message::Text(cdp::navigate_to_history_entry(
                                                next_id, entry_id,
                                            )))
                                            .map_err(|e| e.to_string())?;
                                        next_id += 1;
                                    }
                                    nav_history_step = 0;
                                }
                            }
                        }
                    }
                    if screenshot_request_id == Some(resp_id) {
                        screenshot_request_id = None;
                        if let Some(base64_data) = val
                            .get("result")
                            .and_then(|r| r.get("data"))
                            .and_then(Value::as_str)
                            && let Ok(bytes) = STANDARD.decode(base64_data)
                        {
                            let _ = events.send(BrowserEvent::Screenshot(bytes));
                        }
                    } else if title_request_id == Some(resp_id) {
                        title_request_id = None;
                        if let Some(title) = val
                            .get("result")
                            .and_then(|r| r.get("result"))
                            .and_then(|res| res.get("value"))
                            .and_then(Value::as_str)
                        {
                            page_title = title.to_owned();
                            let _ = events.try_send(BrowserEvent::Title(page_title.clone()));
                        }
                    } else if overlay_request_id == Some(resp_id) {
                        overlay_request_id = None;
                        if let Some(value) = val
                            .get("result")
                            .and_then(|r| r.get("result"))
                            .and_then(|res| res.get("value"))
                            && let Ok(elements) =
                                serde_json::from_value::<Vec<OverlayElement>>(value.clone())
                        {
                            let _ = events.send(BrowserEvent::Elements(elements));
                        }
                    } else if eval_request_ids.remove(&resp_id) {
                        let text = if let Some(err) = val.get("error") {
                            format!("Error: {err}")
                        } else if let Some(res) = val.get("result").and_then(|r| r.get("result")) {
                            res.get("value")
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| res.to_string())
                        } else {
                            "undefined".into()
                        };
                        let _ = events.send(BrowserEvent::Console(ConsoleLog {
                            level: "result".into(),
                            text,
                        }));
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = socket.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => return Ok(SessionResult::Shutdown),
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(4));
            }
            Err(e) => {
                return Err(format!("CDP connection error: {e}"));
            }
            _ => {}
        }
    }
}

fn set_socket_nonblocking(socket: &mut WsStream, nonblocking: bool) -> Result<(), String> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream
            .set_nonblocking(nonblocking)
            .map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_addresses_and_searches() {
        assert_eq!(
            normalize_url("https://example.com/path").unwrap(),
            "https://example.com/path"
        );
        assert_eq!(
            normalize_url("example.com/docs").unwrap(),
            "https://example.com/docs"
        );
        assert_eq!(
            normalize_url("localhost:5173").unwrap(),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_url("127.0.0.1:3000/app").unwrap(),
            "http://127.0.0.1:3000/app"
        );
        assert_eq!(
            normalize_url("komet browser").unwrap(),
            "https://duckduckgo.com/?q=komet%20browser"
        );
        assert_eq!(normalize_url("about:blank").unwrap(), "about:blank");
    }

    #[test]
    fn rejects_empty_and_script_addresses() {
        assert!(normalize_url("  ").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("ftp://example.com").is_err());
        assert!(normalize_url("example.com\n").is_ok());
    }

    #[test]
    fn normalizes_port_only_and_local_tld_and_private_ranges() {
        assert_eq!(normalize_url(":3000").unwrap(), "http://localhost:3000");
        assert_eq!(
            normalize_url("app.localhost:5173").unwrap(),
            "http://app.localhost:5173"
        );
        assert_eq!(
            normalize_url("app.localhost").unwrap(),
            "http://app.localhost"
        );
        assert_eq!(
            normalize_url("172.20.1.5:8080").unwrap(),
            "http://172.20.1.5:8080"
        );
        assert_eq!(normalize_url("172.16.0.1").unwrap(), "http://172.16.0.1");
    }

    #[test]
    fn real_host_parsing_beats_prefix_matching() {
        // localhost.evil.com est un domaine public : jamais de http local.
        assert_eq!(
            normalize_url("localhost.evil.com").unwrap(),
            "https://localhost.evil.com"
        );
        assert_eq!(
            normalize_url("localhost.evil.com:8080").unwrap(),
            "https://localhost.evil.com:8080"
        );
        // 10.999.1 n'est pas une IP des plages privées → domaine public
        assert_eq!(
            normalize_url("192.168.evil.com").unwrap(),
            "https://192.168.evil.com"
        );
    }

    #[test]
    fn search_engine_is_configurable_via_env() {
        // unsafe requis par edition 2024 ; process-global, ok car tests monothread ici.
        unsafe {
            std::env::set_var("KOMET_SEARCH_URL", "https://searx.example/?q={q}");
        }
        assert_eq!(
            normalize_url("komet browser").unwrap(),
            "https://searx.example/?q=komet%20browser"
        );
        unsafe {
            std::env::remove_var("KOMET_SEARCH_URL");
        }
        assert_eq!(
            normalize_url("komet browser").unwrap(),
            "https://duckduckgo.com/?q=komet%20browser"
        );
    }

    #[test]
    fn url_tracker_follows_main_frame_only() {
        let mut tracker = UrlTracker::default();

        // frameNavigated sans parentId = frame principale : fixe l'id + l'URL
        assert_eq!(
            tracker.on_nav_event(
                "Page.frameNavigated",
                &json!({"frame": {"id": "F1", "url": "http://a/"}})
            ),
            Some("http://a/".to_owned())
        );
        assert_eq!(tracker.main_frame_id(), Some("F1"));

        // tilt SPA sur la frame principale : URL dans params.url (PAS params.frame.url)
        assert_eq!(
            tracker.on_nav_event(
                "Page.navigatedWithinDocument",
                &json!({"frameId": "F1", "url": "http://a/route2"})
            ),
            Some("http://a/route2".to_owned())
        );

        // iframe : navigatedWithinDocument ignoré
        assert_eq!(
            tracker.on_nav_event(
                "Page.navigatedWithinDocument",
                &json!({"frameId": "F9", "url": "http://if/x"})
            ),
            None
        );
        // iframe : frameNavigated (avec parentId) ignoré
        assert_eq!(
            tracker.on_nav_event(
                "Page.frameNavigated",
                &json!({"frame": {"id": "F9", "parentId": "F1", "url": "about:blank"}})
            ),
            None
        );
        // la frame principale reste connue après une iframe
        assert_eq!(tracker.main_frame_id(), Some("F1"));
    }

    #[test]
    fn url_tracker_skips_navigated_within_document_when_main_frame_unknown() {
        let mut tracker = UrlTracker::default();
        assert_eq!(
            tracker.on_nav_event(
                "Page.navigatedWithinDocument",
                &json!({"frameId": "F1", "url": "http://a/r"})
            ),
            None
        );
    }

    #[test]
    fn url_tracker_reports_loading_for_main_frame() {
        let mut tracker = UrlTracker::default();
        tracker.on_nav_event(
            "Page.frameNavigated",
            &json!({"frame": {"id": "F1", "url": "http://a/"}}),
        );

        // frame principale : Started → true, Stopped → false
        assert_eq!(
            tracker.on_loading_event("Page.frameStartedLoading", "F1"),
            Some(true)
        );
        assert_eq!(
            tracker.on_loading_event("Page.frameStoppedLoading", "F1"),
            Some(false)
        );
        // autre frame : non pertinent
        assert_eq!(
            tracker.on_loading_event("Page.frameStartedLoading", "F9"),
            None
        );
        assert_eq!(
            tracker.on_loading_event("Page.frameStoppedLoading", "F9"),
            None
        );
        // événement inconnu
        assert_eq!(tracker.on_loading_event("Page.loadEventFired", "F1"), None);
    }

    #[test]
    fn maps_gpui_keys_to_cdp_special_keys() {
        assert_eq!(
            gpui_key_to_cdp("backspace"),
            Some(("Backspace", "Backspace", 8))
        );
        assert_eq!(gpui_key_to_cdp("enter"), Some(("Enter", "Enter", 13)));
        assert_eq!(gpui_key_to_cdp("tab"), Some(("Tab", "Tab", 9)));
        assert_eq!(gpui_key_to_cdp("escape"), Some(("Escape", "Escape", 27)));
        assert_eq!(gpui_key_to_cdp("delete"), Some(("Delete", "Delete", 46)));
        assert_eq!(
            gpui_key_to_cdp("left"),
            Some(("ArrowLeft", "ArrowLeft", 37))
        );
        assert_eq!(
            gpui_key_to_cdp("right"),
            Some(("ArrowRight", "ArrowRight", 39))
        );
        assert_eq!(gpui_key_to_cdp("up"), Some(("ArrowUp", "ArrowUp", 38)));
        assert_eq!(
            gpui_key_to_cdp("down"),
            Some(("ArrowDown", "ArrowDown", 40))
        );
        assert_eq!(gpui_key_to_cdp("home"), Some(("Home", "Home", 36)));
        assert_eq!(gpui_key_to_cdp("end"), Some(("End", "End", 35)));
        // les caractères imprimables passent déjà via Input.char → pas de mapping
        assert_eq!(gpui_key_to_cdp("a"), None);
        assert_eq!(gpui_key_to_cdp("inconnu"), None);
    }

    #[test]
    fn maps_modifier_flags_to_cdp_bits() {
        assert_eq!(cdp_modifiers(false, false, false, false), 0);
        // signature : alt, ctrl, shift, meta
        assert_eq!(cdp_modifiers(false, true, false, false), 2);
        assert_eq!(cdp_modifiers(true, true, false, false), 1 | 2);
        assert_eq!(cdp_modifiers(false, false, true, false), 8);
        assert_eq!(cdp_modifiers(false, false, false, true), 4);
    }

    #[test]
    fn error_text_becomes_friendly_message() {
        let m = navigate_error_message("net::ERR_CONNECTION_REFUSED", "http://localhost:5173/");
        assert!(m.contains("localhost:5173"), "doit citer l'adresse : {m}");
        assert!(
            m.to_lowercase().contains("serveur"),
            "doit suggérer le serveur : {m}"
        );
        assert!(
            m.contains("Réessayer"),
            "doit pointer le bouton Retry : {m}"
        );

        let other = navigate_error_message("net::ERR_NAME_NOT_RESOLVED", "https://nope.example");
        assert!(other.contains("nope.example"), "{other}");
        assert!(
            other.contains("net::ERR_NAME_NOT_RESOLVED"),
            "garde le détail brut : {other}"
        );
    }
}
