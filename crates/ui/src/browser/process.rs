use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command as ProcessCommand, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub struct BrowserProcess {
    child: Child,
}

impl BrowserProcess {
    pub fn spawn(profile_key: &str, initial_url: &str) -> Result<(Self, String), String> {
        let executable = find_browser_executable().ok_or_else(|| {
            "No compatible browser executable found. Please install Chromium, Google Chrome, \
             Brave, Microsoft Edge, or Obscura, or set CHROME_PATH."
                .to_owned()
        })?;

        let profile_dir = browser_profile_path(profile_key);
        let config_dir = profile_dir.join("config");
        let data_dir = profile_dir.join("data");
        let cache_dir = profile_dir.join("cache");
        let crash_dir = profile_dir.join("crashes");
        let _ = std::fs::create_dir_all(&config_dir);
        let _ = std::fs::create_dir_all(&data_dir);
        let _ = std::fs::create_dir_all(&cache_dir);
        let _ = std::fs::create_dir_all(&crash_dir);

        let mut cmd = ProcessCommand::new(&executable);
        cmd.env("XDG_CONFIG_HOME", &config_dir)
            .env("XDG_DATA_HOME", &data_dir)
            .env("XDG_CACHE_HOME", &cache_dir)
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
            .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/dev/null")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");

        cmd.arg("--headless=new")
            .arg("--ozone-platform=headless")
            .arg("--remote-debugging-port=0")
            .arg("--hide-scrollbars")
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-sync")
            .arg("--disable-background-networking")
            .arg("--disable-default-apps")
            .arg("--disable-extensions")
            .arg("--disable-translate")
            .arg("--disable-breakpad")
            .arg("--disable-crash-reporter")
            .arg(format!("--crash-dumps-dir={}", crash_dir.display()))
            .arg("--disable-component-update")
            .arg("--disable-domain-reliability")
            .arg("--disable-client-side-phishing-detection")
            .arg("--mute-audio")
            .arg("--password-store=basic")
            .arg("--use-mock-keychain")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--allow-insecure-localhost")
            .arg(format!("--user-data-dir={}", profile_dir.display()));

        if cfg!(target_os = "linux") {
            cmd.arg("--no-sandbox");
        }

        // initial_url must be the very last positional argument
        cmd.arg(initial_url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "Failed to spawn browser engine at {}: {e}",
                executable.display()
            )
        })?;

        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Browser process stderr not available".into());
        };

        let page_ws_url = match wait_for_cdp_endpoint(&mut child, stderr, initial_url) {
            Ok(url) => url,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };

        Ok((Self { child }, page_ws_url))
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn find_browser_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHROME_PATH")
        .or_else(|| std::env::var_os("KOMET_BROWSER_BIN"))
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Some(path);
    }

    let names = [
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "chromium",
        "brave-browser",
        "microsoft-edge-stable",
        "obscura",
    ];

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let mac_paths = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ];
        for path in mac_paths {
            let candidate = Path::new(path);
            if candidate.is_file() {
                return Some(candidate.to_path_buf());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let win_prefixes = [
            std::env::var("ProgramFiles").ok(),
            std::env::var("ProgramFiles(x86)").ok(),
            std::env::var("LOCALAPPDATA").ok(),
        ];
        let win_relatives = [
            r"Google\Chrome\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
            r"BraveSoftware\Brave-Browser\Application\brave.exe",
            r"Chromium\Application\chrome.exe",
        ];
        for prefix in win_prefixes.into_iter().flatten() {
            for rel in win_relatives {
                let candidate = Path::new(&prefix).join(rel);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let linux_paths = [
            "/usr/lib64/chromium-browser/chromium-browser",
            "/usr/lib/chromium/chromium",
            "/usr/lib/chromium-browser/chromium-browser",
            "/opt/google/chrome/chrome",
            "/opt/microsoft/msedge/msedge",
            "/opt/brave.com/brave/brave-browser",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium-browser",
            "/usr/bin/chromium",
            "/usr/bin/brave-browser",
            "/usr/bin/microsoft-edge-stable",
            "/snap/bin/chromium",
        ];
        for path in linux_paths {
            let candidate = Path::new(path);
            if candidate.is_file() {
                return Some(candidate.to_path_buf());
            }
        }
    }

    None
}

fn wait_for_cdp_endpoint(
    child: &mut Child,
    stderr: ChildStderr,
    initial_url: &str,
) -> Result<String, String> {
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    thread::Builder::new()
        .name("komet-cdp-readiness".into())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(idx) = line.find("DevTools listening on ") {
                    let ws_url = line[idx + "DevTools listening on ".len()..]
                        .trim()
                        .to_owned();
                    if ws_url.starts_with("ws://") {
                        let _ = tx.send(Ok(ws_url));
                        return;
                    }
                }
            }
            let _ = tx.send(Err(
                "Browser exited without outputting CDP WebSocket endpoint".into(),
            ));
        })
        .map_err(|e| format!("Failed to spawn CDP listener thread: {e}"))?;

    let browser_ws = match rx.recv_timeout(Duration::from_secs(25)) {
        Ok(res) => res?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err("Browser CDP endpoint timed out after 25 seconds".into());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let status = child.try_wait().ok().flatten();
            return Err(format!(
                "Browser process exited prematurely (status: {status:?})"
            ));
        }
    };

    let port = browser_ws
        .split(':')
        .nth(2)
        .and_then(|p| p.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| format!("Could not parse port from browser ws URL: {browser_ws}"))?;

    resolve_page_ws_url(&browser_ws, port, initial_url)
}

fn resolve_page_ws_url(browser_ws: &str, port: u16, initial_url: &str) -> Result<String, String> {
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, connect as ws_connect};

    let mut socket = None;
    for _ in 0..20 {
        if let Ok((s, _)) = ws_connect(browser_ws) {
            if let MaybeTlsStream::Plain(stream) = s.get_ref() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
            }
            socket = Some(s);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut socket =
        socket.ok_or_else(|| format!("Failed to connect to browser WebSocket at {browser_ws}"))?;

    socket
        .send(Message::Text(
            r#"{"id":1,"method":"Target.getTargets"}"#.into(),
        ))
        .map_err(|e| e.to_string())?;

    for _ in 0..5 {
        if let Ok(Message::Text(text)) = socket.read() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(targets) = val
                    .get("result")
                    .and_then(|r| r.get("targetInfos"))
                    .and_then(|t| t.as_array())
            {
                for target in targets {
                    if target.get("type").and_then(|t| t.as_str()) == Some("page")
                        && let Some(target_id) = target.get("targetId").and_then(|i| i.as_str())
                    {
                        return Ok(format!("ws://127.0.0.1:{port}/devtools/page/{target_id}"));
                    }
                }
                // targetInfos received but no page target found yet -> proceed immediately to createTarget
                break;
            }
        } else {
            break;
        }
    }

    let create_cmd = serde_json::json!({
        "id": 2,
        "method": "Target.createTarget",
        "params": { "url": initial_url }
    })
    .to_string();
    socket
        .send(Message::Text(create_cmd))
        .map_err(|e| e.to_string())?;

    for _ in 0..10 {
        if let Ok(Message::Text(text)) = socket.read()
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(target_id) = val
                .get("result")
                .and_then(|r| r.get("targetId"))
                .and_then(|i| i.as_str())
        {
            return Ok(format!("ws://127.0.0.1:{port}/devtools/page/{target_id}"));
        }
    }

    Err("Could not resolve page target from browser WebSocket".into())
}

pub fn browser_profile_path(profile_key: &str) -> PathBuf {
    let digest = Sha256::digest(profile_key.as_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    if let Some(data_dir) = std::env::var_os("KOMET_DATA_DIR").map(PathBuf::from) {
        let path = data_dir.join("komet").join("browser-profiles").join(&name);
        if std::fs::create_dir_all(&path).is_ok() {
            return path;
        }
    }

    if let Some(local_data) = dirs::data_local_dir() {
        let path = local_data
            .join("komet")
            .join("browser-profiles")
            .join(&name);
        if std::fs::create_dir_all(&path).is_ok() {
            return path;
        }
    }

    let path = std::env::temp_dir().join("komet-browser").join(&name);
    let _ = std::fs::create_dir_all(&path);
    path
}
