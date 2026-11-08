use std::cell::Cell;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command as ProcessCommand, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gpui::{
    AnyElement, App, Bounds, Context, Element, Entity, FocusHandle, GlobalElementId, ImageSource,
    InspectorElementId, IntoElement, LayoutId, MouseButton, ObjectFit, Pixels, Render, RenderImage,
    ScrollDelta, ScrollWheelEvent, SharedString, Window, div, img, prelude::*, px,
};
use image::{Frame, RgbaImage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::header::AUTHORIZATION;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect as ws_connect};
use uuid::Uuid;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;

const VIEWPORT: (u32, u32) = (1280, 800);
type RpcSocket = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

enum BrowserCommand {
    Back,
    Forward,
    Reload,
    Retry,
    Navigate(String),
    Click { x: f32, y: f32 },
    Scroll { delta_x: f64, delta_y: f64 },
    TypeText(String),
    Shutdown,
}

enum BrowserEvent {
    Page {
        image: Arc<RgbaImage>,
        title: String,
        url: String,
    },
    Failed(String),
}

enum SessionResult {
    Retry,
    Shutdown,
}

struct EngineProcess {
    child: Child,
    endpoint: String,
    token: String,
}

impl EngineProcess {
    fn spawn(profile_key: &str) -> Result<Self, String> {
        let engine = engine_executable();
        let token = Uuid::new_v4().simple().to_string();
        let profile = browser_profile_path(profile_key)?;
        std::fs::create_dir_all(&profile)
            .map_err(|error| format!("Could not create Servo browser profile: {error}"))?;
        let mut child = ProcessCommand::new(&engine)
            .env("SERVO_WEBVIEW_TOKEN", &token)
            .env("SERVO_WEBVIEW_PORT", "0")
            .env("SERVO_WEBVIEW_PROFILE", &profile)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                format!(
                    "Could not start Servo's browser engine at {}: {error}. Build it with \
                     `cargo build --manifest-path ../servo/Cargo.toml \
                     -p webview-engine --bin servo-webview-engine`, then set \
                     SERVO_WEBVIEW_ENGINE to its absolute path.",
                    engine.display()
                )
            })?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Servo browser engine stdout was not available".into());
        };
        let endpoint = match wait_for_ready(&mut child, stdout) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            endpoint,
            token,
        })
    }
}

impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct BrowserSurface {
    commands: Sender<BrowserCommand>,
    events: Receiver<BrowserEvent>,
    address: Entity<ComposerInput>,
    _address_subscription: gpui::Subscription,
    page_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    page_focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    retired_images: Vec<Arc<RenderImage>>,
    title: String,
    url: String,
    error: Option<String>,
}

impl BrowserSurface {
    pub fn new(url: String, profile_key: String, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| ComposerInput::new("Search or enter an address", cx));
        address.update(cx, |input, cx| input.set_text(url.clone(), cx));

        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(1);
        let initial_url = url.clone();
        let startup_events = event_tx.clone();
        if let Err(error) = thread::Builder::new()
            .name("komet-servo-browser".into())
            .spawn(move || run_client(command_rx, event_tx, initial_url, profile_key))
        {
            let _ = startup_events.send(BrowserEvent::Failed(format!(
                "Could not start Servo browser client: {error}"
            )));
        }

        let address_subscription =
            cx.subscribe(&address, |this, input, event: &ComposerInputEvent, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    let value = input.read(cx).text().trim().to_owned();
                    match normalize_url(&value) {
                        Ok(url) => {
                            this.url = url.clone();
                            this.error = None;
                            this.address
                                .update(cx, |input, cx| input.set_text(url.clone(), cx));
                            let _ = this.commands.send(BrowserCommand::Navigate(url));
                        }
                        Err(error) => this.error = Some(error),
                    }
                    cx.notify();
                }
            });

        let browser = Self {
            commands: command_tx,
            events: event_rx,
            address,
            _address_subscription: address_subscription,
            page_bounds: Rc::new(Cell::new(None)),
            page_focus: cx.focus_handle(),
            image: None,
            retired_images: Vec::new(),
            title: "Browser".into(),
            url,
            error: None,
        };

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(32))
                    .await;
                if this.update(cx, |view, cx| view.receive_events(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        browser
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
                    if let Some(previous) = self.image.replace(gpui_image(image)) {
                        self.retired_images.push(previous);
                    }
                    let previous_url = self.url.clone();
                    self.url = url;
                    if self.address.read(cx).text() == previous_url {
                        let address = self.url.clone();
                        self.address
                            .update(cx, |input, cx| input.set_text(address, cx));
                    }
                    self.title = title;
                    self.error = None;
                    changed = true;
                }
                Ok(BrowserEvent::Failed(error)) => {
                    self.error = Some(error);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.error.is_none() {
                        self.error = Some("Servo browser client stopped unexpectedly.".into());
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
        if let Some(text) = event
            .keystroke
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
}

impl Drop for BrowserSurface {
    fn drop(&mut self) {
        let _ = self.commands.send(BrowserCommand::Shutdown);
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired_images.drain(..) {
            ImageSource::Render(image).evict(Some(window), cx);
        }

        let theme = crate::theme::Theme::of(cx).clone();
        let page_ready = self.image.is_some();
        let click_commands = self.commands.clone();
        let scroll_commands = self.commands.clone();
        let page_bounds = self.page_bounds.clone();
        let page_focus = self.page_focus.clone();
        let image = self.image.clone();
        let page = image
            .map(|image| {
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            })
            .unwrap_or_else(|| gpui::Empty.into_any_element());

        let page = BrowserViewport {
            child: div()
                .id("komet-servo-browser-page")
                .size_full()
                .track_focus(&self.page_focus)
                .on_key_down(cx.listener(Self::on_page_key_down))
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    window.focus(&page_focus, cx);
                    if !page_ready {
                        return;
                    }
                    let bounds = page_bounds.get();
                    let Some(bounds) = bounds else {
                        return;
                    };
                    let Some(position) = bounds.localize(&event.position) else {
                        return;
                    };
                    let width = f32::from(bounds.size.width);
                    let height = f32::from(bounds.size.height);
                    let image_scale = (width / VIEWPORT.0 as f32).min(height / VIEWPORT.1 as f32);
                    if image_scale <= 0.0 {
                        return;
                    }
                    let image_x = (width - VIEWPORT.0 as f32 * image_scale) / 2.0;
                    let image_y = (height - VIEWPORT.1 as f32 * image_scale) / 2.0;
                    let x = f32::from(position.x) - image_x;
                    let y = f32::from(position.y) - image_y;
                    if x >= 0.0
                        && y >= 0.0
                        && x < VIEWPORT.0 as f32 * image_scale
                        && y < VIEWPORT.1 as f32 * image_scale
                    {
                        let _ = click_commands.send(BrowserCommand::Click {
                            x: x / image_scale,
                            y: y / image_scale,
                        });
                    }
                })
                .on_scroll_wheel(move |event: &ScrollWheelEvent, _, cx| {
                    let (delta_x, delta_y) = match event.delta {
                        ScrollDelta::Pixels(delta) => (f64::from(delta.x), f64::from(delta.y)),
                        ScrollDelta::Lines(delta) => {
                            (f64::from(delta.x) * 40.0, f64::from(delta.y) * 40.0)
                        }
                    };
                    let _ = scroll_commands.send(BrowserCommand::Scroll { delta_x, delta_y });
                    cx.stop_propagation();
                })
                .child(page)
                .into_any_element(),
            bounds: self.page_bounds.clone(),
        };

        let back = self.commands.clone();
        let forward = self.commands.clone();
        let reload = self.commands.clone();
        let retry = self.error.is_some();

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
                        move || {
                            let _ = back.send(BrowserCommand::Back);
                        },
                    ))
                    .child(browser_button(
                        icons::ARROW_RIGHT,
                        theme.text_muted,
                        move || {
                            let _ = forward.send(BrowserCommand::Forward);
                        },
                    ))
                    .child(browser_button(
                        icons::REFRESH,
                        theme.text_muted,
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
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface)
                            .px(px(6.0))
                            .child(self.address.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(160.0))
                            .overflow_hidden()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(if self.error.is_some() {
                                SharedString::from("Retry")
                            } else {
                                self.title.clone().into()
                            }),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(page))
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
        _state: &mut Self::RequestLayoutState,
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
        _state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

fn browser_button(
    icon_path: &'static str,
    color: gpui::Hsla,
    action: impl Fn() + 'static,
) -> impl IntoElement {
    div()
        .id(icon_path)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::ink(0.06)))
        .on_click(move |_, _, _| action())
        .child(
            crate::icons::icon(icon_path)
                .size(px(14.0))
                .text_color(color),
        )
}

fn gpui_image(image: Arc<RgbaImage>) -> Arc<RenderImage> {
    let mut image = (*image).clone();
    for pixel in image.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![Frame::new(image)]))
}

fn normalize_url(value: &str) -> Result<String, String> {
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
    if !value.chars().any(char::is_whitespace)
        && (value.contains('.')
            || lower.starts_with("localhost")
            || lower.starts_with("127.0.0.1")
            || lower.starts_with("["))
    {
        return Ok(format!("https://{value}"));
    }
    let query = value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    Ok(format!("https://duckduckgo.com/?q={query}"))
}

fn engine_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("SERVO_WEBVIEW_ENGINE") {
        return PathBuf::from(path);
    }
    let executable = if cfg!(windows) {
        "servo-webview-engine.exe"
    } else {
        "servo-webview-engine"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(executable)))
        .unwrap_or_else(|| PathBuf::from(executable))
}

fn connect(endpoint: &str, token: &str) -> Result<RpcSocket, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut request = endpoint
            .to_owned()
            .into_client_request()
            .map_err(|error| error.to_string())?;
        let authorization = format!("Bearer {token}")
            .parse()
            .map_err(|error: tungstenite::http::header::InvalidHeaderValue| error.to_string())?;
        request.headers_mut().insert(AUTHORIZATION, authorization);
        match ws_connect(request) {
            Ok((socket, _)) => return Ok(socket),
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("Could not connect to Servo: {error}")),
        }
    }
}

fn rpc_call(
    socket: &mut RpcSocket,
    next_id: &mut u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = *next_id;
    *next_id += 1;
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string()
            .into(),
        ))
        .map_err(|error| error.to_string())?;
    loop {
        match socket.read().map_err(|error| error.to_string())? {
            Message::Text(response) => {
                let response: Value =
                    serde_json::from_str(response.as_ref()).map_err(|error| error.to_string())?;
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = response.get("error") {
                    return Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown RPC error")
                        .to_owned());
                }
                return Ok(response.get("result").cloned().unwrap_or(Value::Null));
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .map_err(|error| error.to_string())?,
            Message::Close(_) => return Err("Servo RPC connection closed".into()),
            _ => {}
        }
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
    let process = EngineProcess::spawn(profile_key)?;
    let mut socket = connect(&process.endpoint, &process.token)?;
    let mut next_id = 1;
    let space = rpc_call(
        &mut socket,
        &mut next_id,
        "ego.createTaskSpace",
        json!({ "name": "Komet Browser" }),
    )?
    .get("id")
    .and_then(Value::as_u64)
    .ok_or("createTaskSpace response did not contain a space id")?;
    let tab = rpc_call(
        &mut socket,
        &mut next_id,
        "ego.createTab",
        json!({ "space": space, "url": current_url }),
    )?
    .get("id")
    .and_then(Value::as_u64)
    .ok_or("createTab response did not contain a tab id")?;
    let mut next_capture = Instant::now();

    loop {
        match commands.recv_timeout(Duration::from_millis(10)) {
            Ok(BrowserCommand::Back) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.goBack",
                    json!({ "tab": tab }),
                )?;
            }
            Ok(BrowserCommand::Forward) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.goForward",
                    json!({ "tab": tab }),
                )?;
            }
            Ok(BrowserCommand::Reload) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.reload",
                    json!({ "tab": tab }),
                )?;
            }
            Ok(BrowserCommand::Retry) => return Ok(SessionResult::Retry),
            Ok(BrowserCommand::Navigate(url)) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.navigate",
                    json!({ "tab": tab, "url": url }),
                )?;
                *current_url = url;
            }
            Ok(BrowserCommand::Click { x, y }) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.clickAt",
                    json!({ "tab": tab, "x": x, "y": y }),
                )?;
            }
            Ok(BrowserCommand::Scroll { delta_x, delta_y }) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.scroll",
                    json!({ "tab": tab, "deltaX": delta_x, "deltaY": delta_y }),
                )?;
            }
            Ok(BrowserCommand::TypeText(text)) => {
                rpc_call(
                    &mut socket,
                    &mut next_id,
                    "ego.type",
                    json!({ "tab": tab, "text": text }),
                )?;
            }
            Ok(BrowserCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(SessionResult::Shutdown);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if Instant::now() >= next_capture {
            let capture = rpc_call(
                &mut socket,
                &mut next_id,
                "ego.capture",
                json!({ "tab": tab }),
            )?;
            let data = capture
                .get("data")
                .and_then(Value::as_str)
                .ok_or("capture response did not contain image data")?;
            let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
            let image = image::load_from_memory(&bytes)
                .map_err(|error| error.to_string())?
                .to_rgba8();
            let title = capture
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let url = capture
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or(current_url)
                .to_owned();
            if events
                .send(BrowserEvent::Page {
                    image: Arc::new(image),
                    title,
                    url,
                })
                .is_err()
            {
                return Ok(SessionResult::Shutdown);
            }
            next_capture = Instant::now() + Duration::from_millis(120);
        }
    }
}

fn browser_profile_path(profile_key: &str) -> Result<PathBuf, String> {
    let data_dir = dirs::data_local_dir().ok_or_else(|| {
        "No local data directory is available for the browser profile.".to_owned()
    })?;
    let digest = Sha256::digest(profile_key.as_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(data_dir.join("komet").join("servo-browser").join(name))
}

fn wait_for_ready(child: &mut Child, stdout: ChildStdout) -> Result<String, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("komet-servo-engine-ready".into())
        .spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout)
                .read_line(&mut line)
                .map(|bytes| (bytes, line));
            let _ = ready_tx.send(result);
        })
        .map_err(|error| format!("Failed to wait for Servo engine readiness: {error}"))?;

    let line = match ready_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok((0, _))) => {
            let status = child.try_wait().ok().flatten();
            return Err(format!(
                "Servo browser engine exited before readiness (status: {status:?})"
            ));
        }
        Ok(Ok((_, line))) => line,
        Ok(Err(error)) => return Err(format!("Failed to read Servo readiness signal: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err("Servo browser engine did not become ready within 60 seconds".into());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("Servo engine readiness reader stopped unexpectedly".into());
        }
    };
    ready_endpoint(&line)
}

fn ready_endpoint(line: &str) -> Result<String, String> {
    let endpoint = line
        .trim()
        .strip_prefix("SERVO_WEBVIEW_READY ")
        .ok_or_else(|| format!("Unexpected Servo readiness signal: {}", line.trim()))?;
    let address = endpoint
        .strip_prefix("ws://")
        .and_then(|address| address.parse::<SocketAddr>().ok())
        .ok_or_else(|| format!("Invalid Servo endpoint in readiness signal: {endpoint}"))?;
    if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
        return Err(format!(
            "Servo endpoint must use an assigned loopback port: {endpoint}"
        ));
    }
    Ok(endpoint.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            normalize_url("servo browser").unwrap(),
            "https://duckduckgo.com/?q=servo%20browser"
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
    fn readiness_requires_an_assigned_loopback_endpoint() {
        assert_eq!(
            ready_endpoint("SERVO_WEBVIEW_READY ws://127.0.0.1:49152\n").unwrap(),
            "ws://127.0.0.1:49152"
        );
        assert!(ready_endpoint("SERVO_WEBVIEW_READY ws://127.0.0.1:0\n").is_err());
        assert!(ready_endpoint("SERVO_WEBVIEW_READY ws://0.0.0.0:49152\n").is_err());
        assert!(ready_endpoint("READY ws://127.0.0.1:49152\n").is_err());
    }
}
