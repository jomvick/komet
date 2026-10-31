use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::future::join_all;
use komet_proto::HarnessId;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const BINARY_WATCH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const FIRST_RETRY: Duration = Duration::from_secs(5 * 60);
const MAX_JITTER_SECS: u64 = 30 * 60;
/// A manual `checkLatest` right after the checker's own sweep reuses it
/// instead of hitting the registry twice at launch.
const FULL_CHECK_FRESHNESS: Duration = Duration::from_secs(60);

const VERSION_TIMEOUT: Duration = Duration::from_secs(6);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const CURSOR_INSTALLER_URL: &str = "https://cursor.com/install";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCliStatus {
    pub harness: HarnessId,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub managed: bool,
    pub update_available: bool,
    pub check_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NpmVersion {
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManageHarnessCliParams {
    pub harness: HarnessId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCliStatusParams {
    #[serde(default)]
    pub check_latest: bool,
    /// Bypass the short freshness window left by the background checker; the
    /// manual "Check for updates" action wants a live answer.
    #[serde(default)]
    pub force: bool,
    /// Scope the probe to these harnesses. The Providers page asks for the one
    /// card the user opened instead of every installed CLI at once; absent
    /// means the whole catalog (the update check).
    #[serde(default)]
    pub harnesses: Option<Vec<HarnessId>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryFingerprint {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    len: u64,
}

impl BinaryFingerprint {
    fn of(executable: &Path) -> Option<Self> {
        let path = std::fs::canonicalize(executable).ok()?;
        let metadata = path.metadata().ok()?;
        Some(Self {
            path,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

struct CheckerInner {
    harnesses: Vec<HarnessId>,
    statuses: Mutex<HashMap<HarnessId, HarnessCliStatus>>,
    fingerprints: Mutex<HashMap<HarnessId, Option<BinaryFingerprint>>>,
    last_full_check: Mutex<Option<Instant>>,
    refresh_gate: tokio::sync::Mutex<()>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Caches CLI update status and refreshes it independently of the Settings
/// page, including when an agent's own updater replaces its executable.
#[derive(Clone)]
pub struct HarnessCliChecker {
    inner: Arc<CheckerInner>,
}

impl HarnessCliChecker {
    pub fn new(harnesses: Vec<HarnessId>) -> Self {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Self {
            inner: Arc::new(CheckerInner {
                harnesses,
                statuses: Mutex::new(HashMap::new()),
                fingerprints: Mutex::new(HashMap::new()),
                last_full_check: Mutex::new(None),
                refresh_gate: tokio::sync::Mutex::new(()),
                shutdown,
                worker: Mutex::new(None),
            }),
        }
    }

    /// Start the immediate check and background cadence. Idempotent.
    pub fn start(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut worker = lock(&self.inner.worker);
        if worker.is_some() {
            return;
        }
        let checker = self.clone();
        let mut shutdown = self.inner.shutdown.subscribe();
        *worker = Some(runtime.spawn(async move {
            if *shutdown.borrow_and_update() {
                return;
            }
            // Not forced: if the UI's launch check won the gate and swept just
            // now, reuse that result instead of hitting the registry twice.
            let first = tokio::select! {
                _ = shutdown.changed() => return,
                statuses = checker.refresh(true, false) => statuses,
            };
            let mut retry = FIRST_RETRY;
            let mut next_check = Instant::now()
                + if has_check_errors(&first) {
                    retry
                } else {
                    check_delay()
                };
            loop {
                let tick = (Instant::now() + BINARY_WATCH_INTERVAL).min(next_check);
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep_until(tick) => {
                        if Instant::now() >= next_check || checker.binaries_changed() {
                            let statuses = tokio::select! {
                                _ = shutdown.changed() => break,
                                statuses = checker.refresh(true, true) => statuses,
                            };
                            if has_check_errors(&statuses) {
                                next_check = Instant::now() + retry;
                                retry = (retry * 2).min(CHECK_INTERVAL);
                            } else {
                                retry = FIRST_RETRY;
                                next_check = Instant::now() + check_delay();
                            }
                        }
                    }
                }
            }
        }));
    }

    pub async fn statuses(&self, check_latest: bool, force: bool) -> Vec<HarnessCliStatus> {
        if check_latest {
            self.refresh(true, force).await
        } else {
            let cached = self.cached_statuses();
            if cached.len() == self.inner.harnesses.len() {
                cached
            } else {
                self.refresh(false, force).await
            }
        }
    }

    /// Statuses for a subset: cached entries answer instantly, anything else is
    /// probed for just those harnesses and merged into the shared cache.
    pub async fn statuses_for(
        &self,
        harnesses: &[HarnessId],
        check_latest: bool,
    ) -> Vec<HarnessCliStatus> {
        if !check_latest {
            let cached: Vec<HarnessCliStatus> = {
                let statuses = lock(&self.inner.statuses);
                harnesses
                    .iter()
                    .filter_map(|harness| statuses.get(harness).cloned())
                    .collect()
            };
            if cached.len() == harnesses.len() {
                return cached;
            }
        }
        let statuses = super::harness_cli::statuses(harnesses, check_latest).await;
        {
            let mut cached = lock(&self.inner.statuses);
            for status in &statuses {
                cached.insert(status.harness, status.clone());
            }
        }
        statuses
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.send_replace(true);
        let worker = lock(&self.inner.worker).take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }

    fn cached_statuses(&self) -> Vec<HarnessCliStatus> {
        let statuses = lock(&self.inner.statuses);
        self.inner
            .harnesses
            .iter()
            .filter_map(|harness| statuses.get(harness).cloned())
            .collect()
    }

    fn recently_checked(&self) -> bool {
        lock(&self.inner.last_full_check)
            .is_some_and(|checked| checked.elapsed() < FULL_CHECK_FRESHNESS)
    }

    async fn refresh(&self, check_latest: bool, force: bool) -> Vec<HarnessCliStatus> {
        let _refresh = self.inner.refresh_gate.lock().await;
        if !check_latest {
            let cached = self.cached_statuses();
            if cached.len() == self.inner.harnesses.len() {
                return cached;
            }
        } else if !force && self.recently_checked() {
            // The check is done while holding the gate, so a caller that
            // waited out the background sweep reuses its result instead of
            // hitting the registry again.
            let cached = self.cached_statuses();
            if cached.len() == self.inner.harnesses.len() {
                return cached;
            }
        }
        let statuses = super::harness_cli::statuses(&self.inner.harnesses, check_latest).await;
        {
            let mut cached = lock(&self.inner.statuses);
            for status in &statuses {
                cached.insert(status.harness, status.clone());
            }
        }
        if check_latest {
            *lock(&self.inner.last_full_check) = Some(Instant::now());
        }
        let fingerprints = self
            .inner
            .harnesses
            .iter()
            .copied()
            .map(|harness| (harness, binary_fingerprint(harness)))
            .collect();
        *lock(&self.inner.fingerprints) = fingerprints;
        statuses
    }

    fn binaries_changed(&self) -> bool {
        let previous = lock(&self.inner.fingerprints);
        !previous.is_empty()
            && self.inner.harnesses.iter().any(|harness| {
                previous
                    .get(harness)
                    .is_some_and(|fingerprint| fingerprint != &binary_fingerprint(*harness))
            })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn binary_fingerprint(harness: HarnessId) -> Option<BinaryFingerprint> {
    komet_harness::find_cli_path(executable(harness)).and_then(|path| BinaryFingerprint::of(&path))
}

fn has_check_errors(statuses: &[HarnessCliStatus]) -> bool {
    statuses.iter().any(|status| status.check_error.is_some())
}

fn check_delay() -> Duration {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % MAX_JITTER_SECS;
    CHECK_INTERVAL + Duration::from_secs(jitter)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallSource {
    /// The CLI's own updater, run against the binary Komet resolved.
    SelfUpdate(&'static [&'static str]),
    Npm(&'static str),
    CursorOfficialInstaller,
    ShellInstaller(&'static str),
    HermesInstaller,
}

/// The public release feed for a CLI. It is intentionally separate from the
/// update mechanism: Claude, Codex, OpenCode and Pi publish to npm but update
/// through their own updater.
fn npm_package(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::ClaudeCode => Some("@anthropic-ai/claude-code"),
        HarnessId::Codex => Some("@openai/codex"),
        HarnessId::Grok => Some("@xai-official/grok"),
        HarnessId::Opencode => Some("opencode-ai"),
        HarnessId::Pi => Some("@earendil-works/pi-coding-agent"),
        HarnessId::Copilot => Some("@github/copilot"),
        HarnessId::Cline => Some("cline"),
        HarnessId::Qoder => Some("@qoder-ai/qodercli"),
        _ => None,
    }
}

fn install_source(harness: HarnessId) -> Option<InstallSource> {
    Some(match harness {
        HarnessId::ClaudeCode => InstallSource::SelfUpdate(&["update"]),
        HarnessId::Codex => InstallSource::SelfUpdate(&["update"]),
        HarnessId::Cursor => InstallSource::CursorOfficialInstaller,
        HarnessId::Devin => InstallSource::ShellInstaller("https://cli.devin.ai/install.sh"),
        HarnessId::Grok => InstallSource::Npm("@xai-official/grok"),
        HarnessId::Hermes => InstallSource::HermesInstaller,
        HarnessId::Opencode => InstallSource::SelfUpdate(&["upgrade"]),
        HarnessId::Pi => InstallSource::SelfUpdate(&["update", "--self"]),
        HarnessId::Antigravity => {
            InstallSource::ShellInstaller("https://antigravity.google/cli/install.sh")
        }
        HarnessId::Muse => InstallSource::ShellInstaller("https://dev.meta.ai/install.sh"),
        HarnessId::Copilot => InstallSource::Npm("@github/copilot"),
        HarnessId::Cline => InstallSource::Npm("cline"),
        // The recommended install is Qoder's own script; the CLI's built-in
        // updater works for both script and npm installs.
        HarnessId::Qoder => InstallSource::SelfUpdate(&["update"]),
        HarnessId::Mock => return None,
    })
}

fn supported_on_platform(source: InstallSource) -> bool {
    #[cfg(windows)]
    {
        !matches!(
            source,
            InstallSource::CursorOfficialInstaller
                | InstallSource::ShellInstaller(_)
                | InstallSource::HermesInstaller
        )
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        true
    }
}

fn executable(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor-agent",
        HarnessId::Devin => "devin",
        HarnessId::Grok => "grok",
        HarnessId::Hermes => "hermes",
        HarnessId::Opencode => "opencode",
        HarnessId::Pi => "pi",
        HarnessId::Antigravity => "agy",
        HarnessId::Muse => "muse",
        HarnessId::Copilot => "copilot",
        HarnessId::Cline => "cline",
        HarnessId::Qoder => "qoder",
        HarnessId::Mock => "mock",
    }
}

pub async fn statuses(harnesses: &[HarnessId], check_latest: bool) -> Vec<HarnessCliStatus> {
    let client = check_latest.then(version_client);
    join_all(
        harnesses
            .iter()
            .copied()
            .map(|harness| status(harness, client.as_ref())),
    )
    .await
}

async fn status(
    harness: HarnessId,
    client: Option<&Result<reqwest::Client, String>>,
) -> HarnessCliStatus {
    let source = install_source(harness);
    let installed_version = version(harness).await;
    let feed = match (installed_version.is_some(), client) {
        (true, Some(client)) => match npm_package(harness) {
            Some(package) => Some(match client {
                Ok(client) => npm_latest(client, package).await,
                Err(error) => Err(error.clone()),
            }),
            None if harness == HarnessId::Cursor => Some(match client {
                Ok(client) => cursor_latest(client).await,
                Err(error) => Err(error.clone()),
            }),
            None => None,
        },
        _ => None,
    };
    let (latest_version, check_error) = match feed {
        Some(Ok(version)) => (Some(version), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    let update_available = installed_version
        .as_deref()
        .zip(latest_version.as_deref())
        .is_some_and(|(installed, latest)| is_newer(latest, installed));
    HarnessCliStatus {
        harness,
        installed_version,
        latest_version,
        managed: source.is_some_and(supported_on_platform),
        update_available,
        check_error,
    }
}

async fn version(harness: HarnessId) -> Option<String> {
    let path = komet_harness::find_cli_path(executable(harness))?;
    let mut command = tokio::process::Command::new(path);
    command.arg("--version");
    komet_harness::compose_login_shell_path(&mut command);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    let output = output
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(80)
        .collect::<String>();
    (!output.is_empty()).then_some(output)
}

fn version_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(VERSION_TIMEOUT)
        .user_agent(concat!("Komet/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| format!("Could not create the version-check client: {err}"))
}

fn npm_latest_url(package: &str) -> String {
    format!(
        "https://registry.npmjs.org/{}/latest",
        package.replace('/', "%2F")
    )
}

async fn npm_latest(client: &reqwest::Client, package: &str) -> Result<String, String> {
    let response = client
        .get(npm_latest_url(package))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("Could not check the latest CLI version: {err}"))?;
    let latest: NpmVersion = response
        .json()
        .await
        .map_err(|err| format!("The package registry returned an invalid version: {err}"))?;
    Ok(latest.version)
}

async fn cursor_latest(client: &reqwest::Client) -> Result<String, String> {
    let response = client
        .get(CURSOR_INSTALLER_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("Could not check the latest Cursor CLI version: {err}"))?;
    let script = response
        .text()
        .await
        .map_err(|err| format!("Could not read the Cursor installer: {err}"))?;
    let version = script
        .split("/lab/")
        .nth(1)
        .and_then(|tail| tail.split('/').next())
        .filter(|version| !version.is_empty())
        .ok_or_else(|| "The Cursor installer did not include a version.".to_string())?;
    Ok(version.to_string())
}

fn version_token(version: &str) -> &str {
    version
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(version)
}

fn is_newer(latest: &str, installed: &str) -> bool {
    let latest = semver::Version::parse(latest.trim_start_matches('v'));
    let installed = semver::Version::parse(version_token(installed).trim_start_matches('v'));
    match (latest, installed) {
        (Ok(latest), Ok(installed)) => latest > installed,
        _ => false,
    }
}

pub async fn manage(harness: HarnessId) -> Result<(), String> {
    let installed_before = version(harness).await;
    // Fetched before the update: the feed is the target the UI promised.
    let latest = if installed_before.is_some() {
        latest_version(harness).await.ok()
    } else {
        None
    };
    match install_source(harness) {
        Some(InstallSource::SelfUpdate(args)) => {
            // A self-updater only updates an existing install; a missing CLI
            // is installed through its published npm package instead.
            if komet_harness::find_cli_path(executable(harness)).is_some() {
                self_update(harness, args).await?;
            } else if let Some(package) = npm_package(harness) {
                install_npm(package).await?;
            } else {
                return Err(format!(
                    "Could not find the {} CLI to install.",
                    harness.display_name()
                ));
            }
        }
        Some(InstallSource::Npm(package)) => install_npm(package).await?,
        Some(InstallSource::CursorOfficialInstaller) => install_cursor().await?,
        Some(InstallSource::ShellInstaller(url)) => install_shell_script(url).await?,
        Some(InstallSource::HermesInstaller) => install_hermes().await?,
        None => {
            return Err(format!(
                "Komet does not have a verified automatic installer for {} yet.",
                harness.display_name()
            ));
        }
    }
    let installed_after = version(harness).await;
    // Only a harness with a release feed can prove an unchanged version is stale.
    let has_feed = npm_package(harness).is_some() || harness == HarnessId::Cursor;
    verify_update(
        installed_before.as_deref(),
        installed_after.as_deref(),
        latest.as_deref(),
        has_feed,
    )
}

/// Some updaters exit cleanly without replacing anything, so success is the
/// version the CLI reports afterwards. The feed is authoritative when it is
/// reachable; for a harness with a feed but no reachable feed, the version must
/// at least have moved. A feedless harness can legitimately reinstall the same
/// version, so reporting one is enough.
fn verify_update(
    installed_before: Option<&str>,
    installed_after: Option<&str>,
    latest: Option<&str>,
    has_feed: bool,
) -> Result<(), String> {
    let Some(after) = installed_after else {
        return Err(if installed_before.is_some() {
            "The CLI did not report a version after updating.".to_string()
        } else {
            "The CLI did not report a version after installation.".to_string()
        });
    };
    if let Some(latest) = latest {
        if is_newer(latest, after) {
            return Err(format!(
                "Still on {} after updating to {latest}.",
                version_token(after)
            ));
        }
    } else if has_feed && installed_before == Some(after) {
        return Err(format!("Still on {} after updating.", version_token(after)));
    }
    Ok(())
}

async fn latest_version(harness: HarnessId) -> Result<String, String> {
    let client = version_client()?;
    if let Some(package) = npm_package(harness) {
        npm_latest(&client, package).await
    } else if harness == HarnessId::Cursor {
        cursor_latest(&client).await
    } else {
        Err(format!(
            "Komet has no public version feed for {}.",
            harness.display_name()
        ))
    }
}

/// Each CLI's own updater knows how it was installed (native, Homebrew, npm)
/// better than Komet could guess from the binary path.
async fn self_update(harness: HarnessId, args: &'static [&'static str]) -> Result<(), String> {
    let path = komet_harness::find_cli_path(executable(harness)).ok_or_else(|| {
        format!(
            "Could not find the {} CLI to update.",
            harness.display_name()
        )
    })?;
    let mut command = tokio::process::Command::new(path);
    command
        .args(args)
        // An updater that stops to ask must fail, not hang.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    komet_harness::compose_login_shell_path(&mut command);
    let output = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| "The CLI update timed out after five minutes.".to_string())?
        .map_err(|err| {
            format!(
                "Could not start the {} updater: {err}",
                harness.display_name()
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(update_failure(&output.stdout, &output.stderr))
}

/// Updaters print their reason to either stream; the last line is the one
/// that says what went wrong.
fn update_failure(stdout: &[u8], stderr: &[u8]) -> String {
    [stderr, stdout]
        .iter()
        .filter_map(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .map(str::to_string)
        })
        .next()
        .unwrap_or_else(|| "Update failed".to_string())
}

async fn install_npm(package: &str) -> Result<(), String> {
    let mut command = tokio::process::Command::new("npm");
    command
        .args(["install", "--global", &format!("{package}@latest")])
        .kill_on_drop(true);
    komet_harness::compose_login_shell_path(&mut command);
    let output = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| "The CLI installation timed out after five minutes.".to_string())?
        .map_err(|err| format!("Could not start npm: {err}"))?;
    ensure_success(output, "npm install")
}

async fn install_shell_script(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = url;
        return Err(
            "Automatic CLI installation through shell scripts is not available on Windows.".into(),
        );
    }
    #[cfg(not(windows))]
    {
        let client = reqwest::Client::builder()
            .timeout(VERSION_TIMEOUT)
            .user_agent(concat!("Komet/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("Could not create the installer client: {err}"))?;
        let response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| format!("Could not download the official CLI installer: {err}"))?;
        if response
            .content_length()
            .is_some_and(|size| size > 1_000_000)
        {
            return Err("The official CLI installer was unexpectedly large.".into());
        }
        let script = response
            .text()
            .await
            .map_err(|err| format!("Could not read the official CLI installer: {err}"))?;
        if script.len() > 1_000_000 || !script.starts_with("#!") {
            return Err("The official CLI installer had an unexpected format.".into());
        }
        let interpreter = if script.starts_with("#!/bin/sh") {
            "sh"
        } else {
            "bash"
        };
        let mut command = tokio::process::Command::new(interpreter);
        command
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        komet_harness::compose_login_shell_path(&mut command);
        let mut child = command
            .spawn()
            .map_err(|err| format!("Could not start the official CLI installer: {err}"))?;
        use tokio::io::AsyncWriteExt;
        child
            .stdin
            .take()
            .ok_or_else(|| "Could not provide the official CLI installer.".to_string())?
            .write_all(script.as_bytes())
            .await
            .map_err(|err| format!("Could not provide the official CLI installer: {err}"))?;
        let output = tokio::time::timeout(INSTALL_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "The CLI installation timed out after five minutes.".to_string())?
            .map_err(|err| format!("Could not complete the CLI installer: {err}"))?;
        ensure_success(output, "Official CLI installer")
    }
}

async fn install_hermes() -> Result<(), String> {
    install_shell_script("https://hermes-agent.nousresearch.com/install.sh").await?;
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Err("The Hermes installer completed, but HOME is unavailable.".into());
    };
    let mut command = tokio::process::Command::new("uv");
    command
        .args(["pip", "install", "-e", ".[acp]"])
        .current_dir(home.join(".hermes/hermes-agent"))
        .kill_on_drop(true);
    komet_harness::compose_login_shell_path(&mut command);
    let output = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| "Hermes ACP setup timed out after five minutes.".to_string())?
        .map_err(|err| format!("Could not start uv for Hermes ACP setup: {err}"))?;
    ensure_success(output, "Hermes ACP setup")
}

async fn install_cursor() -> Result<(), String> {
    #[cfg(windows)]
    {
        return Err(
            "Automatic Cursor CLI installation is not yet available on Windows.".to_string(),
        );
    }
    #[cfg(not(windows))]
    {
        let client = reqwest::Client::builder()
            .timeout(VERSION_TIMEOUT)
            .user_agent(concat!("Komet/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("Could not create the installer client: {err}"))?;
        let response = client
            .get(CURSOR_INSTALLER_URL)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|err| format!("Could not download the official Cursor installer: {err}"))?;
        if response
            .content_length()
            .is_some_and(|size| size > 1_000_000)
        {
            return Err("The official Cursor installer was unexpectedly large.".into());
        }
        let script = response
            .text()
            .await
            .map_err(|err| format!("Could not read the official Cursor installer: {err}"))?;
        if script.len() > 1_000_000 || !script.starts_with("#!/usr/bin/env bash") {
            return Err("The official Cursor installer had an unexpected format.".into());
        }
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        komet_harness::compose_login_shell_path(&mut command);
        let mut child = command
            .spawn()
            .map_err(|err| format!("Could not start the Cursor installer: {err}"))?;
        use tokio::io::AsyncWriteExt;
        child
            .stdin
            .take()
            .ok_or_else(|| "Could not provide the Cursor installer script.".to_string())?
            .write_all(script.as_bytes())
            .await
            .map_err(|err| format!("Could not provide the Cursor installer script: {err}"))?;
        let output = tokio::time::timeout(INSTALL_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "The Cursor CLI installation timed out after five minutes.".to_string())?
            .map_err(|err| format!("Could not complete the Cursor installer: {err}"))?;
        ensure_success(output, "Cursor installer")
    }
}

fn ensure_success(output: std::process::Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if stderr.is_empty() { stdout } else { stderr };
    Err(if details.is_empty() {
        format!("{operation} exited with status {}.", output.status)
    } else {
        format!("{operation} failed: {details}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manages_only_known_official_cli_sources() {
        assert!(install_source(HarnessId::ClaudeCode).is_some());
        assert!(install_source(HarnessId::Cursor).is_some());
        assert!(install_source(HarnessId::Grok).is_some());
        assert!(install_source(HarnessId::Muse).is_some());
        assert!(install_source(HarnessId::Antigravity).is_some());
        assert!(install_source(HarnessId::Mock).is_none());
    }

    #[test]
    fn every_cli_with_a_self_updater_uses_it_instead_of_npm() {
        assert_eq!(
            install_source(HarnessId::ClaudeCode),
            Some(InstallSource::SelfUpdate(&["update"]))
        );
        assert_eq!(
            install_source(HarnessId::Codex),
            Some(InstallSource::SelfUpdate(&["update"]))
        );
        assert_eq!(
            install_source(HarnessId::Opencode),
            Some(InstallSource::SelfUpdate(&["upgrade"]))
        );
        assert_eq!(
            install_source(HarnessId::Pi),
            Some(InstallSource::SelfUpdate(&["update", "--self"]))
        );
        assert_eq!(
            install_source(HarnessId::Qoder),
            Some(InstallSource::SelfUpdate(&["update"]))
        );
        // These have no self-updater, so npm stays the fallback.
        assert_eq!(
            install_source(HarnessId::Grok),
            Some(InstallSource::Npm("@xai-official/grok"))
        );
        assert_eq!(
            install_source(HarnessId::Copilot),
            Some(InstallSource::Npm("@github/copilot"))
        );
        assert_eq!(
            install_source(HarnessId::Cline),
            Some(InstallSource::Npm("cline"))
        );
    }

    #[test]
    fn every_self_updating_cli_has_an_npm_install_fallback() {
        for harness in [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Opencode,
            HarnessId::Pi,
            HarnessId::Qoder,
        ] {
            assert!(matches!(
                install_source(harness),
                Some(InstallSource::SelfUpdate(_))
            ));
            assert!(
                npm_package(harness).is_some(),
                "{} must remain installable when its CLI is missing",
                harness.display_name()
            );
        }
    }

    #[test]
    fn feeds_pi_releases_from_the_package_its_installer_ships() {
        assert_eq!(
            npm_package(HarnessId::Pi),
            Some("@earendil-works/pi-coding-agent")
        );
        assert_eq!(npm_package(HarnessId::Cursor), None);
        assert_eq!(npm_package(HarnessId::Mock), None);
        assert_eq!(npm_package(HarnessId::Opencode), Some("opencode-ai"));
    }

    #[test]
    fn verifies_the_reported_version_after_an_update() {
        assert!(verify_update(Some("2.0.0"), Some("2.0.1"), Some("2.0.1"), true).is_ok());
        assert_eq!(
            verify_update(Some("2.0.0"), Some("2.0.0"), Some("2.0.1"), true).unwrap_err(),
            "Still on 2.0.0 after updating to 2.0.1."
        );
        // A harness with a feed but no reachable feed must at least move.
        assert!(verify_update(Some("2.0.0"), Some("2.0.1"), None, true).is_ok());
        assert_eq!(
            verify_update(Some("2.0.0"), Some("2.0.0"), None, true).unwrap_err(),
            "Still on 2.0.0 after updating."
        );
        // A feedless harness may reinstall the same version.
        assert!(verify_update(Some("2.0.0"), Some("2.0.0"), None, false).is_ok());
        // A fresh install only needs to report a version.
        assert!(verify_update(None, Some("1.0.0"), None, false).is_ok());
        assert_eq!(
            verify_update(None, None, None, false).unwrap_err(),
            "The CLI did not report a version after installation."
        );
    }

    #[test]
    fn reports_the_last_line_an_updater_printed() {
        assert_eq!(
            update_failure(
                b"checking\n",
                b"npm ERR! code EACCES\nnpm ERR! permission denied\n\n"
            ),
            "npm ERR! permission denied"
        );
        assert_eq!(update_failure(b"no write access\n", b""), "no write access");
        assert_eq!(update_failure(b"", b""), "Update failed");
    }

    #[test]
    fn compares_semver_versions_without_guessing_unparseable_versions() {
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.2.3", "1.2.3 (Claude Code)"));
        assert!(!is_newer("latest", "unknown"));
    }

    #[test]
    fn npm_scoped_package_urls_preserve_the_scope_marker() {
        assert_eq!(
            npm_latest_url("@anthropic-ai/claude-code"),
            "https://registry.npmjs.org/@anthropic-ai%2Fclaude-code/latest"
        );
        assert_eq!(
            npm_latest_url("opencode-ai"),
            "https://registry.npmjs.org/opencode-ai/latest"
        );
    }

    #[test]
    fn latest_version_checks_are_opt_in() {
        let default_params: HarnessCliStatusParams = serde_json::from_value(serde_json::json!({}))
            .expect("empty status params should preserve the local-only default");
        assert!(!default_params.check_latest);
        assert!(!default_params.force);

        let explicit_params: HarnessCliStatusParams =
            serde_json::from_value(serde_json::json!({ "checkLatest": true, "force": true }))
                .unwrap();
        assert!(explicit_params.check_latest);
        assert!(explicit_params.force);
    }

    #[test]
    fn status_params_can_scope_to_the_opened_card() {
        let all: HarnessCliStatusParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(all.harnesses, None);

        let one: HarnessCliStatusParams =
            serde_json::from_value(serde_json::json!({ "harnesses": ["codex"] })).unwrap();
        assert_eq!(one.harnesses, Some(vec![HarnessId::Codex]));
    }

    #[test]
    fn extracts_cursor_version_from_the_official_installer_source() {
        let script = r#"DOWNLOAD_URL="https://downloads.cursor.com/lab/2026.10.22-build/linux/x64/agent-cli-package.tar.gz""#;
        let version = script
            .split("/lab/")
            .nth(1)
            .and_then(|tail| tail.split('/').next());
        assert_eq!(version, Some("2026.10.22-build"));
    }

    #[cfg(unix)]
    #[test]
    fn binary_fingerprint_changes_when_an_installer_replaces_the_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("agent");
        std::fs::write(&executable, b"old").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let before = BinaryFingerprint::of(&executable).unwrap();

        std::fs::write(&executable, b"replacement").unwrap();

        let after = BinaryFingerprint::of(&executable).unwrap();
        assert_ne!(before, after);
    }
}
