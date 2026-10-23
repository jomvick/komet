//! Browser-use preset (port of freebuff's `agents/browser-use`): a pinned
//! `chrome-devtools-mcp` stdio server (`--headless --isolated`) that any
//! `supports_dynamic_mcp` harness drives with its own subagent/tool loop, plus
//! the file-system-only Chrome/npx probes that gate enabling it.
//!
//! No new binary, no new `ToolCall` variant: tool calls surface as
//! `ToolCall::Mcp { server: "chrome-devtools", .. }` and the agent's final
//! reply carries the structured report parsed by
//! [`komet_proto::browser_use::BrowserUseReport`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::config::{McpServerConfig, McpTransport};

pub const SERVER_ID: &str = "chrome-devtools";
pub const SERVER_NAME: &str = "Browser use (Chrome DevTools)";
/// Pinned rather than `@latest`: reproducible tool surface and no silent
/// supply-chain drift on every spawn.
pub const CHROME_DEVTOOLS_MCP_VERSION: &str = "1.12.0";
/// Env override honoured by the probe (same name chrome-devtools-mcp /
/// puppeteer read), so a custom Chrome location works without a settings UI.
pub const CHROME_PATH_ENV: &str = "CHROME_PATH";

pub fn package_spec() -> String {
    format!("chrome-devtools-mcp@{CHROME_DEVTOOLS_MCP_VERSION}")
}

/// The registry entry. `always_load` is the caller's distribution choice:
/// `true` injects the browser into every run of a compatible harness, `false`
/// keeps it a per-chat opt-in via the usual MCP assignment.
pub fn server_config(always_load: bool) -> McpServerConfig {
    McpServerConfig {
        id: SERVER_ID.into(),
        name: SERVER_NAME.into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec![
            "-y".into(),
            package_spec(),
            "--headless".into(),
            "--isolated".into(),
        ],
        url: None,
        headers: Default::default(),
        env: Default::default(),
        always_load,
    }
}

pub fn is_browser_use(config: &McpServerConfig) -> bool {
    config.id == SERVER_ID
}

/// Point the preset at Komet's own Chrome instead of letting the MCP spawn a
/// private headless instance: the agent then drives the tab the Browser
/// surface displays. `--isolated`/`--headless` are dropped (they only apply
/// to a self-launched browser).
pub fn attach_to_browser(config: &mut McpServerConfig, browser_url: &str) {
    config.args = vec![
        "-y".into(),
        package_spec(),
        "--browserUrl".into(),
        browser_url.to_string(),
    ];
}

/// Snapshot for `BrowserUseStatus` — no secrets, safe for RPC/UI.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUseStatus {
    pub chrome_installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chrome_path: Option<String>,
    pub npx_installed: bool,
    /// Registered in the MCP registry (enabled or not).
    pub configured: bool,
    pub enabled: bool,
    pub always_load: bool,
    pub package: String,
    /// Human-readable reason the preset cannot run, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

pub fn status(registry: &super::McpRegistry) -> BrowserUseStatus {
    let chrome = probe_chrome();
    let npx = probe_npx();
    let entry = registry.get(SERVER_ID);
    BrowserUseStatus {
        chrome_installed: chrome.is_some(),
        chrome_path: chrome.map(|p| p.display().to_string()),
        npx_installed: npx.is_some(),
        configured: entry.is_some(),
        enabled: entry.as_ref().is_some_and(|c| c.enabled),
        always_load: entry.as_ref().is_some_and(|c| c.always_load),
        package: package_spec(),
        blocker: blocker(),
    }
}

/// `Some(reason)` when enabling would only produce a dead `npx` process.
pub fn blocker() -> Option<String> {
    if probe_chrome().is_none() {
        return Some(format!(
            "Google Chrome ou Chromium introuvable. Installez Chrome (ou définissez \
             {CHROME_PATH_ENV} vers l'exécutable) avant d'activer le navigateur."
        ));
    }
    if probe_npx().is_none() {
        return Some(
            "npx introuvable (Node.js requis pour lancer chrome-devtools-mcp). \
             Installez Node.js ≥ 20 avant d'activer le navigateur."
                .into(),
        );
    }
    None
}

pub fn probe_npx() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "npx.cmd" } else { "npx" };
    komet_harness::find_on_paths(exe, Vec::new())
}

/// File-system probe only — never spawns Chrome.
pub fn probe_chrome() -> Option<PathBuf> {
    probe_chrome_with(
        |key| std::env::var_os(key),
        |p| p.is_file() || p.is_dir(),
        |exe| komet_harness::find_on_paths(exe, Vec::new()),
    )
}

/// Injectable core of [`probe_chrome`]: `env` reads a variable, `exists`
/// checks a candidate path, `on_path` resolves a bare executable name.
pub fn probe_chrome_with(
    env: impl Fn(&str) -> Option<OsString>,
    exists: impl Fn(&Path) -> bool,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(custom) = env(CHROME_PATH_ENV).filter(|v| !v.is_empty()) {
        let custom = PathBuf::from(custom);
        return exists(&custom).then_some(custom);
    }
    for candidate in well_known_paths(&env) {
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    PATH_NAMES.iter().find_map(|exe| on_path(exe))
}

/// Bare executable names tried on PATH after the well-known locations.
const PATH_NAMES: &[&str] = if cfg!(windows) {
    &["chrome.exe", "msedge.exe"]
} else {
    &[
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
        "brave-browser",
        "microsoft-edge",
    ]
};

fn well_known_paths(env: &impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            "Chromium.app/Contents/MacOS/Chromium",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ];
        for app in apps {
            out.push(PathBuf::from("/Applications").join(app));
            if let Some(home) = env("HOME") {
                out.push(PathBuf::from(home).join("Applications").join(app));
            }
        }
    } else if cfg!(windows) {
        let rel = [
            "Google\\Chrome\\Application\\chrome.exe",
            "Chromium\\Application\\chrome.exe",
            "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
            "Microsoft\\Edge\\Application\\msedge.exe",
        ];
        for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(base) = env(root) {
                for r in rel {
                    out.push(PathBuf::from(&base).join(r));
                }
            }
        }
    } else {
        for p in [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
            "/usr/bin/brave-browser",
            "/usr/bin/microsoft-edge",
            "/opt/google/chrome/chrome",
        ] {
            out.push(PathBuf::from(p));
        }
        if let Some(home) = env("HOME") {
            out.push(PathBuf::from(home).join(".cache/puppeteer"));
        }
    }
    out
}

/// System prompt ported from freebuff's `browser-use` agent. Komet has no
/// per-agent system prompt slot, so this is prepended to the spawned
/// subagent's task by [`task_prompt`] and exposed via `BrowserUseStatus` for
/// prompt-library seeding.
pub const SYSTEM_PROMPT: &str = r#"You are an expert browser automation agent. You use Chrome DevTools MCP tools (server `chrome-devtools`) to navigate web pages, interact with elements, and verify application behavior.

## Available Browser Tools

### Navigation
- **navigate_page**: Load a URL in the browser
- **select_page**: Switch between open tabs

### Inspection (USE THESE FIRST)
- **take_snapshot**: Text representation of the page's accessibility tree with unique element uids. **Always use this before interacting with elements.**
- **take_screenshot**: Visual screenshot of the current page, for layout/styling checks the accessibility tree cannot capture.

### Interaction
- **click**: Click a page element (uid from snapshot)
- **fill**: Type text into an input (`{ "uid": "...", "value": "text" }`)
- **hover**: Trigger hover effects
- **press_key**: Press a key on a focused element (`{ "uid": "...", "key": "Enter" }`)

### Debugging
- **list_console_messages**: Browser console output (errors, warnings, logs)
- **list_network_requests** / **get_network_request**: Network activity
- **evaluate_script**: Run JavaScript in the page (see "evaluate_script Usage")

### Performance
- **performance_start_trace** / **performance_stop_trace**

## Critical Workflow Rules

1. **Snapshot first**: after navigating or any DOM-changing action, call `take_snapshot` BEFORE clicking or filling.
2. **Wait for page loads**: after `navigate_page`, snapshot to confirm the page is ready.
3. **Batch form interactions**: fill several fields and click in sequence without re-snapshotting — uids stay stable while the DOM is unchanged. Re-snapshot only after navigation / submission.
4. **Verify with snapshots**: after key interactions, `take_snapshot` to confirm the result; `take_screenshot` for visual checks.
5. **Error recovery**: if a click or fill fails, take a new snapshot — uids may have changed.
6. **Console monitoring**: `list_console_messages` after loads and interactions.
7. **Be systematic**: Navigate → Snapshot → Plan → Act → Verify → Report.
8. **Prefer snapshots over evaluate_script** for extracting text; use `evaluate_script` only for real JavaScript logic (computed styles, scroll positions, DOM manipulation).

## Element Targeting

Snapshots return elements with `uid` strings like "1_11". `click` and `fill` REQUIRE that `uid`. Always extract it from the latest snapshot.

## evaluate_script Usage

The `function` parameter MUST be an arrow function or function expression — the server wraps it in parentheses and calls it.
✅ `{ "function": "() => { return document.title }" }`
✅ `{ "function": "async () => { const r = await fetch('/api'); return await r.json() }" }`
✅ `{ "function": "(el) => { return el.innerText }", "args": ["1_11"] }`
❌ `{ "function": "document.title" }` (bare expression)
❌ `{ "function": "return document.title" }` (bare return)
The return value must be JSON-serializable.

## Keyboard Shortcuts

Prefer keyboard actions over clicking: after filling a search box, `press_key` with `{ "uid": "...", "key": "Enter" }` — search buttons are often hidden.

## Report

When done, end your reply with ONE JSON object (in a ```json fence) and nothing after it:
{
  "overallStatus": "success" | "failure" | "partial",
  "summary": "what was done, key observations, outcome",
  "finalUrl": "url when finished",
  "finalPageTitle": "page title when finished",
  "results": [{ "name": "step", "passed": true, "details": "what happened", "url": "optional" }],
  "consoleErrors": [{ "message": "error", "url": "optional" }],
  "lessons": ["advice for future runs: timing, unexpected behaviour, workarounds"]
}
`overallStatus`, `summary` and `results` are required."#;

/// Prompt a parent agent (or the composer) sends to the browser subagent:
/// system prompt + task + optional starting URL.
pub fn task_prompt(task: &str, url: Option<&str>) -> String {
    let mut out = String::from(SYSTEM_PROMPT);
    out.push_str("\n\n## Your Task\n\n");
    out.push_str(task.trim());
    if let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) {
        out.push_str(&format!("\n\nStarting URL: {url}"));
    }
    out.push_str(
        "\n\nWorkflow: navigate → snapshot → act → verify → list_console_messages → report.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn server_config_is_pinned_headless_isolated_stdio() {
        let cfg = server_config(false);
        assert_eq!(cfg.id, SERVER_ID);
        assert_eq!(cfg.transport, McpTransport::Stdio);
        assert_eq!(cfg.command.as_deref(), Some("npx"));
        assert_eq!(
            cfg.args,
            vec![
                "-y",
                &format!("chrome-devtools-mcp@{CHROME_DEVTOOLS_MCP_VERSION}"),
                "--headless",
                "--isolated"
            ]
        );
        assert!(!cfg.args.iter().any(|a| a.contains("@latest")));
        assert!(cfg.env.is_empty() && cfg.headers.is_empty());
        cfg.validate().unwrap();
        assert!(server_config(true).always_load);
        assert!(is_browser_use(&cfg));
    }

    #[test]
    fn attach_to_browser_swaps_launch_flags_for_browser_url() {
        let mut cfg = server_config(false);
        attach_to_browser(&mut cfg, "http://127.0.0.1:9222");
        assert_eq!(
            cfg.args,
            vec![
                "-y",
                &package_spec(),
                "--browserUrl",
                "http://127.0.0.1:9222"
            ]
        );
        assert!(
            !cfg.args
                .iter()
                .any(|a| a == "--isolated" || a == "--headless")
        );
    }

    #[test]
    fn chrome_path_env_wins_and_must_exist() {
        let env = |k: &str| (k == CHROME_PATH_ENV).then(|| OsString::from("/custom/chrome"));
        let found = probe_chrome_with(env, |p| p == Path::new("/custom/chrome"), |_| None);
        assert_eq!(found, Some(PathBuf::from("/custom/chrome")));
        // A dangling override does not fall through to the defaults.
        let found = probe_chrome_with(env, |_| true, |_| Some(PathBuf::from("/usr/bin/chrome")));
        assert_eq!(found, Some(PathBuf::from("/custom/chrome")));
        let found = probe_chrome_with(env, |_| false, |_| Some(PathBuf::from("/usr/bin/chrome")));
        assert_eq!(found, None);
    }

    #[test]
    fn falls_back_to_well_known_then_path_names() {
        let found = probe_chrome_with(no_env, |_| false, |_| None);
        assert_eq!(found, None);
        let found = probe_chrome_with(
            no_env,
            |_| false,
            |exe| (exe == "chromium").then(|| PathBuf::from("/nix/bin/chromium")),
        );
        assert_eq!(found, Some(PathBuf::from("/nix/bin/chromium")));
        let known = well_known_paths(&no_env);
        assert!(!known.is_empty());
        let first = known[0].clone();
        let found = probe_chrome_with(no_env, |p| p == first, |_| None);
        assert_eq!(found, Some(first));
    }

    #[test]
    fn task_prompt_carries_system_prompt_task_and_url() {
        let p = task_prompt("Check the login form", Some(" http://localhost:3000 "));
        assert!(p.starts_with(SYSTEM_PROMPT));
        assert!(p.contains("## Your Task\n\nCheck the login form"));
        assert!(p.contains("Starting URL: http://localhost:3000"));
        assert!(!task_prompt("x", None).contains("Starting URL"));
        assert!(SYSTEM_PROMPT.contains("\"overallStatus\""));
    }
}
