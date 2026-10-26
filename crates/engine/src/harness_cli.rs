use std::time::Duration;

use futures::future::join_all;
use komet_proto::HarnessId;
use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Copy)]
enum InstallSource {
    Npm(&'static str),
    CursorOfficialInstaller,
    ShellInstaller(&'static str),
    HermesInstaller,
}

fn install_source(harness: HarnessId) -> Option<InstallSource> {
    Some(match harness {
        HarnessId::ClaudeCode => InstallSource::Npm("@anthropic-ai/claude-code"),
        HarnessId::Codex => InstallSource::Npm("@openai/codex"),
        HarnessId::Cursor => InstallSource::CursorOfficialInstaller,
        HarnessId::Devin => InstallSource::ShellInstaller("https://cli.devin.ai/install.sh"),
        HarnessId::Grok => InstallSource::Npm("@xai-official/grok"),
        HarnessId::Hermes => InstallSource::HermesInstaller,
        HarnessId::Opencode => InstallSource::Npm("opencode-ai"),
        HarnessId::Pi => InstallSource::Npm("@mariozechner/pi-coding-agent"),
        HarnessId::Antigravity => {
            InstallSource::ShellInstaller("https://antigravity.google/cli/install.sh")
        }
        HarnessId::Muse => InstallSource::ShellInstaller("https://dev.meta.ai/install.sh"),
        HarnessId::Copilot => InstallSource::Npm("@github/copilot"),
        HarnessId::Cline => InstallSource::Npm("cline"),
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
    let mut check_error = None;
    let latest_version = match (installed_version.is_some(), source, client) {
        (true, Some(InstallSource::Npm(package)), Some(client)) => {
            let result = match client {
                Ok(client) => npm_latest(client, package).await,
                Err(error) => Err(error.clone()),
            };
            match result {
                Ok(version) => Some(version),
                Err(error) => {
                    check_error = Some(error);
                    None
                }
            }
        }
        (true, Some(InstallSource::CursorOfficialInstaller), Some(client)) => {
            let result = match client {
                Ok(client) => cursor_latest(client).await,
                Err(error) => Err(error.clone()),
            };
            match result {
                Ok(version) => Some(version),
                Err(error) => {
                    check_error = Some(error);
                    None
                }
            }
        }
        _ => None,
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

fn is_newer(latest: &str, installed: &str) -> bool {
    let latest = semver::Version::parse(latest.trim_start_matches('v'));
    let installed = installed
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(installed)
        .trim_start_matches('v');
    match (latest, semver::Version::parse(installed)) {
        (Ok(latest), Ok(installed)) => latest > installed,
        _ => false,
    }
}

pub async fn manage(harness: HarnessId) -> Result<(), String> {
    match install_source(harness) {
        Some(InstallSource::Npm(package)) => install_npm(package).await,
        Some(InstallSource::CursorOfficialInstaller) => install_cursor().await,
        Some(InstallSource::ShellInstaller(url)) => install_shell_script(url).await,
        Some(InstallSource::HermesInstaller) => install_hermes().await,
        None => Err(format!(
            "Komet does not have a verified automatic installer for {} yet.",
            harness.display_name()
        )),
    }
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

        let explicit_params: HarnessCliStatusParams =
            serde_json::from_value(serde_json::json!({ "checkLatest": true })).unwrap();
        assert!(explicit_params.check_latest);
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
}
