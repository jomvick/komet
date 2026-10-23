//! Komet's own headless Chrome (issue #45): one process per engine, driven
//! over the Chrome DevTools Protocol. Two consumers share it:
//!
//! - the UI's Browser surface (navigate, screenshots, clicks) via the
//!   `Browser*` RPCs, so a pasted link is actually rendered on screen;
//! - agents, through `chrome-devtools-mcp --browserUrl <this chrome>` —
//!   [`crate::mcp::browser_use`] rewrites the preset at run time so the agent
//!   drives the SAME tab the user is looking at, like an integrated
//!   browser-use harness.
//!
//! Chrome is launched lazily (first navigate or first run with the preset),
//! isolated in a throwaway profile, and killed with the engine.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
pub const VIEWPORT: (u32, u32) = (1280, 800);

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("{0}")]
    NotInstalled(String),
    #[error("chrome launch failed: {0}")]
    Launch(String),
    #[error("devtools: {0}")]
    Protocol(String),
    #[error("browser is not running")]
    NotRunning,
}

/// What the UI needs to paint one frame.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserFrame {
    pub url: String,
    pub title: String,
    /// JPEG, base64 — `None` while no page has been opened yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_base64: Option<String>,
    pub width: u32,
    pub height: u32,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInfo {
    pub running: bool,
    /// `http://127.0.0.1:<port>` — what `chrome-devtools-mcp --browserUrl` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_url: Option<String>,
}

struct Live {
    child: tokio::process::Child,
    browser_url: String,
    profile_dir: tempfile::TempDir,
    /// Page-level websocket of the tab both the UI and the agent drive. A
    /// page socket follows the tab across renderer swaps, where a flattened
    /// browser-socket session detaches on the first cross-site navigation.
    cdp: Arc<Cdp>,
}

#[derive(Default)]
pub struct BrowserHost {
    live: Mutex<Option<Live>>,
}

/// Process-wide singleton — one Chrome per engine, like the MCP registry it
/// pairs with.
pub fn host() -> &'static Arc<BrowserHost> {
    static HOST: std::sync::OnceLock<Arc<BrowserHost>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| Arc::new(BrowserHost::default()))
}

impl BrowserHost {
    pub async fn info(&self) -> BrowserInfo {
        let live = self.live.lock().await;
        BrowserInfo {
            running: live.is_some(),
            browser_url: live.as_ref().map(|l| l.browser_url.clone()),
        }
    }

    /// Start Chrome if needed and return its DevTools HTTP endpoint.
    pub async fn ensure_started(&self) -> Result<String, BrowserError> {
        let mut live = self.live.lock().await;
        if let Some(l) = live.as_mut() {
            match l.child.try_wait() {
                Ok(None) => return Ok(l.browser_url.clone()),
                _ => {
                    tracing::warn!("komet chrome exited; relaunching");
                    *live = None;
                }
            }
        }
        let started = launch().await?;
        let url = started.browser_url.clone();
        *live = Some(started);
        Ok(url)
    }

    pub async fn navigate(&self, url: &str) -> Result<BrowserFrame, BrowserError> {
        self.ensure_started().await?;
        let cdp = self.session().await?;
        cdp.call(None, "Page.navigate", serde_json::json!({ "url": url }))
            .await?;
        // Give the page a beat to paint before the first frame; the UI keeps
        // polling afterwards.
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.frame_with(&cdp).await
    }

    pub async fn history(&self, delta: i64) -> Result<BrowserFrame, BrowserError> {
        let cdp = self.session().await?;
        let hist = cdp
            .call(None, "Page.getNavigationHistory", serde_json::json!({}))
            .await?;
        let current = hist["currentIndex"].as_i64().unwrap_or(0);
        let entries = hist["entries"].as_array().cloned().unwrap_or_default();
        let target = current + delta;
        if target >= 0
            && let Some(entry) = entries.get(target as usize)
            && let Some(id) = entry["id"].as_i64()
        {
            cdp.call(
                None,
                "Page.navigateToHistoryEntry",
                serde_json::json!({ "entryId": id }),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.frame_with(&cdp).await
    }

    pub async fn reload(&self) -> Result<BrowserFrame, BrowserError> {
        let cdp = self.session().await?;
        cdp.call(None, "Page.reload", serde_json::json!({})).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.frame_with(&cdp).await
    }

    pub async fn frame(&self) -> Result<BrowserFrame, BrowserError> {
        let cdp = self.session().await?;
        self.frame_with(&cdp).await
    }

    /// Forward a UI click (viewport coordinates) to the page.
    pub async fn click(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        let cdp = self.session().await?;
        for kind in ["mousePressed", "mouseReleased"] {
            cdp.call(
                None,
                "Input.dispatchMouseEvent",
                serde_json::json!({
                    "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1
                }),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn scroll(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<(), BrowserError> {
        let cdp = self.session().await?;
        cdp.call(
            None,
            "Input.dispatchMouseEvent",
            serde_json::json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }),
        )
        .await?;
        Ok(())
    }

    /// Type text into the focused element (after a click).
    pub async fn insert_text(&self, text: &str) -> Result<(), BrowserError> {
        let cdp = self.session().await?;
        cdp.call(
            None,
            "Input.insertText",
            serde_json::json!({ "text": text }),
        )
        .await?;
        Ok(())
    }

    pub async fn press_key(&self, key: &str) -> Result<(), BrowserError> {
        let cdp = self.session().await?;
        let (code, vk) = match key {
            "enter" => ("Enter", 13),
            "backspace" => ("Backspace", 8),
            "tab" => ("Tab", 9),
            "escape" => ("Escape", 27),
            _ => return Ok(()),
        };
        for kind in ["keyDown", "keyUp"] {
            cdp.call(
                None,
                "Input.dispatchKeyEvent",
                serde_json::json!({
                    "type": kind, "key": code, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
                    "text": if key == "enter" && kind == "keyDown" { "\r" } else { "" }
                }),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        let mut live = self.live.lock().await;
        if let Some(mut l) = live.take() {
            let _ = l.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), l.child.wait()).await;
            drop(l.profile_dir);
        }
    }

    async fn session(&self) -> Result<Arc<Cdp>, BrowserError> {
        let live = self.live.lock().await;
        let l = live.as_ref().ok_or(BrowserError::NotRunning)?;
        Ok(l.cdp.clone())
    }

    async fn frame_with(&self, cdp: &Cdp) -> Result<BrowserFrame, BrowserError> {
        let hist = cdp
            .call(None, "Page.getNavigationHistory", serde_json::json!({}))
            .await?;
        let current = hist["currentIndex"].as_i64().unwrap_or(0);
        let entries = hist["entries"].as_array().cloned().unwrap_or_default();
        let entry = entries.get(current as usize).cloned().unwrap_or_default();
        let url = entry["url"].as_str().unwrap_or("about:blank").to_string();
        let title = entry["title"].as_str().unwrap_or("").to_string();
        let image_base64 = if url == "about:blank" {
            None
        } else {
            let shot = cdp
                .call(
                    None,
                    "Page.captureScreenshot",
                    serde_json::json!({ "format": "jpeg", "quality": 70 }),
                )
                .await?;
            shot["data"].as_str().map(str::to_string)
        };
        Ok(BrowserFrame {
            url,
            title,
            image_base64,
            width: VIEWPORT.0,
            height: VIEWPORT.1,
            can_go_back: current > 0,
            can_go_forward: (current as usize + 1) < entries.len(),
        })
    }
}

async fn launch() -> Result<Live, BrowserError> {
    let chrome = crate::mcp::browser_use::probe_chrome().ok_or_else(|| {
        BrowserError::NotInstalled(
            "Google Chrome ou Chromium introuvable (définissez CHROME_PATH).".into(),
        )
    })?;
    let profile_dir = tempfile::Builder::new()
        .prefix("komet-chrome-")
        .tempdir()
        .map_err(|e| BrowserError::Launch(e.to_string()))?;
    let mut cmd = tokio::process::Command::new(&chrome);
    cmd.args([
        "--headless=new",
        "--remote-debugging-port=0",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-gpu",
        "--disable-dev-shm-usage",
        "--hide-scrollbars",
        &format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1),
        &format!("--user-data-dir={}", profile_dir.path().display()),
        "about:blank",
    ]);
    // Root-only containers refuse the Chrome sandbox; everywhere else keep it.
    if is_root() {
        cmd.arg("--no-sandbox");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| BrowserError::Launch(format!("{}: {e}", chrome.display())))?;
    let stderr = child.stderr.take().expect("piped");
    let ws_url = tokio::time::timeout(LAUNCH_TIMEOUT, async {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(rest) = line.trim().strip_prefix("DevTools listening on ") {
                return Some(rest.trim().to_string());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(|| BrowserError::Launch("no DevTools endpoint announced".into()))?;
    // Drain the rest of stderr so Chrome never blocks on a full pipe.
    let browser_url = http_from_ws(&ws_url);
    let browser = Cdp::connect(&ws_url).await?;
    let target_id = browser.first_page_target().await?;
    drop(browser);
    let cdp = Cdp::connect(&format!(
        "{}/devtools/page/{target_id}",
        ws_url.split("/devtools/").next().unwrap_or(&ws_url)
    ))
    .await?;
    cdp.prepare_page().await?;
    Ok(Live {
        child,
        browser_url,
        profile_dir,
        cdp,
    })
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// `ws://127.0.0.1:PORT/devtools/browser/<id>` → `http://127.0.0.1:PORT`.
pub fn http_from_ws(ws: &str) -> String {
    let rest = ws.trim_start_matches("ws://");
    let host = rest.split('/').next().unwrap_or(rest);
    format!("http://{host}")
}

/// Minimal CDP client over the browser websocket: flat sessions, request ids,
/// events ignored (the UI polls frames instead of subscribing).
struct Cdp {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    pending: std::sync::Mutex<std::collections::HashMap<u64, oneshot::Sender<serde_json::Value>>>,
    next_id: AtomicU64,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<Arc<Self>, BrowserError> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| BrowserError::Protocol(format!("connect {ws_url}: {e}")))?;
        let (mut sink, mut source) = stream.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let cdp = Arc::new(Self {
            tx,
            pending: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        });
        let reader = Arc::downgrade(&cdp);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = source.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(id) = value["id"].as_u64() else {
                    continue;
                };
                let Some(cdp) = reader.upgrade() else { break };
                let waiter = cdp.pending.lock().ok().and_then(|mut p| p.remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(value);
                }
            }
        });
        Ok(cdp)
    }

    async fn call(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BrowserError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = serde_json::json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session_id {
            msg["sessionId"] = serde_json::Value::String(s.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| BrowserError::Protocol("pending map poisoned".into()))?
            .insert(id, tx);
        self.tx
            .send(msg.to_string())
            .map_err(|_| BrowserError::Protocol("websocket closed".into()))?;
        let reply = tokio::time::timeout(CALL_TIMEOUT, rx)
            .await
            .map_err(|_| BrowserError::Protocol(format!("{method}: timeout")))?
            .map_err(|_| BrowserError::Protocol(format!("{method}: connection dropped")))?;
        if let Some(err) = reply.get("error") {
            return Err(BrowserError::Protocol(format!(
                "{method}: {}",
                err["message"].as_str().unwrap_or("error")
            )));
        }
        Ok(reply["result"].clone())
    }

    /// The initial about:blank tab's id (created if Chrome came up empty).
    async fn first_page_target(&self) -> Result<String, BrowserError> {
        let targets = self
            .call(None, "Target.getTargets", serde_json::json!({}))
            .await?;
        let page = targets["targetInfos"]
            .as_array()
            .and_then(|t| t.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str().map(str::to_string));
        match page {
            Some(id) => Ok(id),
            None => {
                let created = self
                    .call(
                        None,
                        "Target.createTarget",
                        serde_json::json!({ "url": "about:blank" }),
                    )
                    .await?;
                created["targetId"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| BrowserError::Protocol("createTarget: no id".into()))
            }
        }
    }

    /// Page socket setup: enable Page events and pin the viewport.
    async fn prepare_page(&self) -> Result<(), BrowserError> {
        self.call(None, "Page.enable", serde_json::json!({}))
            .await?;
        self.call(
            None,
            "Emulation.setDeviceMetricsOverride",
            serde_json::json!({
                "width": VIEWPORT.0, "height": VIEWPORT.1,
                "deviceScaleFactor": 1, "mobile": false
            }),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_url_from_devtools_ws() {
        assert_eq!(
            http_from_ws("ws://127.0.0.1:37319/devtools/browser/9f39"),
            "http://127.0.0.1:37319"
        );
    }

    /// Real Chrome round-trip; needs Chrome on the machine (CHROME_PATH or
    /// a well-known path).
    #[tokio::test]
    #[ignore]
    async fn navigates_and_screenshots_real_chrome() {
        let host = BrowserHost::default();
        let frame = host
            .navigate("data:text/html,<title>Komet</title><h1>hello</h1>")
            .await
            .expect("navigate");
        assert_eq!(frame.title, "Komet");
        assert!(
            frame
                .image_base64
                .as_deref()
                .is_some_and(|b| b.len() > 1000)
        );
        assert!(host.info().await.browser_url.is_some());
        host.shutdown().await;
    }
}
