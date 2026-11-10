use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command as ProcessCommand, Stdio};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Chrome partagé par profil : une entrée par `profile_key`, refcountée par
/// surface. Retirer l'entrée (0 refs) drop le sender de kill → le thread
/// keeper tue le process. Le keeper détient le Child et ne meurt qu'avec lui,
/// donc PR_SET_PDEATHSIG tue Chrome si komet plante (pas d'orphelin).
struct PoolEntry {
    refs: u32,
    browser_ws: String,
    // seul son drop compte : retirer l'entrée tue le Chrome via le keeper
    _kill: mpsc::Sender<()>,
}

static POOL: LazyLock<Mutex<HashMap<String, PoolEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Sérialise les acquires : deux surfaces ouvertes en rafale ne doivent pas
/// lancer deux Chrome sur le même profil (le verrou user-data-dir fait
/// déléguer le 2e au 1er, qui meurt sans endpoint CDP).
static SPAWN_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Une lease sur le Chrome du profil : le drop relâche une ref ; à 0 le
/// process est tué. `(page_ws_url, warning_sandbox, lease)`.
pub struct PooledBrowser {
    profile_key: String,
}

impl Drop for PooledBrowser {
    fn drop(&mut self) {
        let Ok(mut pool) = POOL.lock() else {
            return;
        };
        if let Some(entry) = pool.get_mut(&self.profile_key)
            && release_ref(&mut entry.refs) == Refcount::Last
        {
            pool.remove(&self.profile_key);
        }
    }
}

pub fn acquire_browser(
    profile_key: &str,
    initial_url: &str,
) -> Result<(String, Option<String>, PooledBrowser), String> {
    let _serial = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 1. Un Chrome vit déjà pour ce profil → on y ouvre notre page target.
    let existing = POOL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(profile_key)
        .map(|e| e.browser_ws.clone());
    if let Some(browser_ws) = existing {
        match create_page_target(&browser_ws, initial_url) {
            Ok(page_ws) => {
                if let Some(entry) = POOL
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(profile_key)
                {
                    acquire_ref(&mut entry.refs);
                }
                return Ok((
                    page_ws,
                    None,
                    PooledBrowser {
                        profile_key: profile_key.to_owned(),
                    },
                ));
            }
            // Chrome mort entre-temps : purge, on repart sur un spawn neuf.
            Err(_) => {
                POOL.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(profile_key);
            }
        }
    }

    // 2. Spawn neuf (thread keeper + readiness + fallback sandbox).
    let (page_ws, browser_ws, warning, kill) = spawn_pooled_browser(profile_key, initial_url)?;
    match POOL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(profile_key.to_owned())
    {
        Entry::Vacant(slot) => {
            slot.insert(PoolEntry {
                refs: 1,
                browser_ws,
                _kill: kill,
            });
        }
        Entry::Occupied(_) => unreachable!("SPAWN_LOCK sérialise les acquires"),
    }
    Ok((
        page_ws,
        warning,
        PooledBrowser {
            profile_key: profile_key.to_owned(),
        },
    ))
}

fn spawn_pooled_browser(
    profile_key: &str,
    initial_url: &str,
) -> Result<(String, String, Option<String>, mpsc::Sender<()>), String> {
    let (kill_tx, kill_rx) = mpsc::channel::<()>();
    let (reply_tx, reply_rx) =
        mpsc::sync_channel::<Result<(String, String, Option<String>), String>>(1);
    let key = profile_key.to_owned();
    let initial = initial_url.to_owned();

    thread::Builder::new()
        .name("komet-browser-keeper".into())
        .spawn(move || {
            // sandbox-first : on tente AVEC sandbox ; --no-sandbox uniquement
            // en repli si Chrome refuse de démarrer sandboxé.
            let launched = match launch_browser(&key, false, &initial) {
                Ok((child, ws)) => Ok((child, ws, None)),
                Err(e) if needs_sandbox_fallback(&e) => {
                    launch_browser(&key, true, &initial).map(|(child, ws)| (child, ws, Some(e)))
                }
                Err(e) => Err(e),
            };
            let (mut child, browser_ws, warning) = match launched {
                Ok(ok) => ok,
                Err(e) => {
                    let _ = reply_tx.send(Err(e));
                    return;
                }
            };
            let page_ws = match port_of(&browser_ws)
                .and_then(|port| resolve_page_ws_url(&browser_ws, port, &initial))
            {
                Ok(ws) => ws,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reply_tx.send(Err(e));
                    return;
                }
            };
            if reply_tx.send(Ok((page_ws, browser_ws, warning))).is_err() {
                // plus d'acquéreur : personne n'utilisera ce Chrome
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            // keeper : un signal, des senders droppés ou la mort de komet
            // (PDEATHSIG) arrêtent le process.
            let _ = kill_rx.recv();
            let _ = child.kill();
            let _ = child.wait();
        })
        .map_err(|e| format!("Failed to spawn browser keeper thread: {e}"))?;

    // couvre readiness 25 s + fallback sandbox (Chrome sandboxé échoue en <1 s)
    reply_rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| "Browser CDP startup timed out or keeper died".to_owned())
        .and_then(|inner| inner)
        .map(|(page_ws, browser_ws, warning)| (page_ws, browser_ws, warning, kill_tx))
}

fn launch_browser(
    profile_key: &str,
    no_sandbox: bool,
    initial_url: &str,
) -> Result<(Child, String), String> {
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

    if no_sandbox {
        cmd.arg("--no-sandbox");
    }

    #[cfg(target_os = "linux")]
    // Chrome meurt avec le thread keeper (donc avec komet) — pas d'orphelin.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            Ok(())
        });
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

    match wait_for_cdp_endpoint(&mut child, stderr) {
        Ok(browser_ws) => Ok((child, browser_ws)),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
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

fn wait_for_cdp_endpoint(child: &mut Child, stderr: ChildStderr) -> Result<String, String> {
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    thread::Builder::new()
        .name("komet-cdp-readiness".into())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            // l'extrait stderr sert à reconnaître un échec de sandbox
            // (needs_sandbox_fallback) et à diagnostiquer les autres cas
            let mut excerpt = String::new();
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
                if excerpt.len() < 4096 {
                    excerpt.push_str(&line);
                    excerpt.push('\n');
                }
            }
            let _ = tx.send(Err(format!(
                "Browser exited without outputting CDP WebSocket endpoint. stderr:\n{excerpt}"
            )));
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

    Ok(browser_ws)
}

fn port_of(browser_ws: &str) -> Result<u16, String> {
    browser_ws
        .split(':')
        .nth(2)
        .and_then(|p| p.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| format!("Could not parse port from browser ws URL: {browser_ws}"))
}

/// Ouvre un NOUVEAU page target sur un Chrome déjà lancé (le pool partage un
/// Chrome par profil : chaque surface a sa propre cible).
fn create_page_target(browser_ws: &str, url: &str) -> Result<String, String> {
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, connect as ws_connect};

    let port = port_of(browser_ws)?;
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

    let create_cmd = serde_json::json!({
        "id": 1,
        "method": "Target.createTarget",
        "params": { "url": url }
    })
    .to_string();
    socket
        .send(Message::Text(create_cmd))
        .map_err(|e| e.to_string())?;

    for _ in 0..10 {
        if let Ok(Message::Text(text)) = socket.read()
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(target_id) = val
                .pointer("/result/targetId")
                .and_then(serde_json::Value::as_str)
        {
            return Ok(format!("ws://127.0.0.1:{port}/devtools/page/{target_id}"));
        }
    }
    Err("Could not create page target on shared browser".into())
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

// stubs RED (TDD #76) — implémentation dans la phase GREEN
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refcount {
    Kept,
    Last,
}

fn acquire_ref(refs: &mut u32) {
    *refs += 1;
}

fn release_ref(refs: &mut u32) -> Refcount {
    if *refs > 1 {
        *refs -= 1;
        Refcount::Kept
    } else {
        *refs = 0;
        Refcount::Last
    }
}

fn needs_sandbox_fallback(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("failed to move to new namespace") || e.contains("suid sandbox")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn pool_refcount_kills_only_on_last_release() {
        let mut refs: HashMap<String, u32> = HashMap::new();
        // deux surfaces sur le même profil partagent le même Chrome
        acquire_ref(refs.entry("p".into()).or_insert(0));
        acquire_ref(refs.entry("p".into()).or_insert(0));
        assert_eq!(refs["p"], 2, "deux surfaces = 2 refs");
        assert_eq!(release_ref(refs.get_mut("p").unwrap()), Refcount::Kept);
        assert_eq!(refs["p"], 1);
        assert_eq!(release_ref(refs.get_mut("p").unwrap()), Refcount::Last);
        assert_eq!(refs["p"], 0, "dernière release = 0 ref (le pool kill)");
    }

    #[test]
    fn detects_sandbox_failure_signatures() {
        assert!(needs_sandbox_fallback(
            "Failed to move to new namespace: PID namespaces supported, Network namespace supported, but failed: errno = Operation not permitted"
        ));
        assert!(needs_sandbox_fallback(
            "Running without the SUID sandbox! Read more at https://chromium.googlesource.com/"
        ));
        assert!(needs_sandbox_fallback(
            "The SUID sandbox helper binary was found, but is not configured correctly."
        ));
        assert!(!needs_sandbox_fallback("net::ERR_CONNECTION_REFUSED"));
        assert!(!needs_sandbox_fallback(""));
    }
}
