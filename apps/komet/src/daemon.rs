//! `komet daemon …` — install/manage `komet headless` as a background service:
//! a systemd **user** unit on Linux (the VPS deployment target), a launchd
//! LaunchAgent on macOS, a native Windows service on Windows. The unit runs
//! the current executable with the `KOMET_*` environment captured at install
//! time, so `KOMET_EDGE_URL=… komet daemon install` bakes that override in.
//!
//! Auth is decoupled: without a saved session the service remains up on the
//! local-only profile. `komet login` and a service restart opt into sync.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

/// Windows service-manager implementation. Declared unconditionally: the
/// `cfg!(target_os = "windows")` branches below are type-checked on every
/// platform, so the module (and its non-Windows stubs) must exist everywhere.
mod win;

const LAUNCHD_LABEL: &str = "sh.komet.app";
/// Same unit name the curl|sh installer (`edge/src/install.sh`) writes, so
/// `komet daemon …` manages that installation rather than a competing copy.
const SYSTEMD_UNIT: &str = "komet.service";

/// Windows service key name (SCM database) and the registry service subkey.
const SERVICE_NAME: &str = "Komet";

/// SCM entry point for the hidden `--service` flag (see `win`).
#[cfg(target_os = "windows")]
pub fn run_as_windows_service() -> anyhow::Result<()> {
    win::run_as_windows_service()
}

#[cfg(not(target_os = "windows"))]
pub fn run_as_windows_service() -> anyhow::Result<()> {
    bail!("--service is only used internally by the Windows service manager")
}

/// Re-apply the env persisted at install to the process environment before
/// anything reads it (logging filter, engine config).
#[cfg(target_os = "windows")]
pub fn load_persisted_env() {
    win::load_persisted_env();
}

#[cfg(not(target_os = "windows"))]
pub fn load_persisted_env() {}

/// Environment captured into the unit file. `PATH` is always included (the
/// engine spawns harness CLIs like `claude`, which service managers' minimal
/// default PATH won't find); the `KOMET_*`/logging vars only when set.
const CAPTURED_ENV: &[&str] = &[
    "PATH",
    "KOMET_DATA_DIR",
    "KOMET_EDGE_URL",
    "KOMET_EDGE_TOKEN",
    "KOMET_ORG_ID",
    "KOMET_WORKOS_CLIENT_ID",
    "KOMET_WORKOS_API_BASE",
    "KOMET_IPC_PORT",
    "KOMET_CALLBACK_PORT",
    "KOMET_HARNESS",
    "KOMET_DEVICE_NAME",
    "RUST_LOG",
];

pub fn install(data_dir: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving the komet executable path")?;
    let env = captured_env();
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path()?;
        std::fs::create_dir_all(plist.parent().expect("LaunchAgents parent"))?;
        std::fs::create_dir_all(data_dir)?;
        // Reinstall-friendly: unload any previous incarnation before rewriting.
        let _ = run_quiet("launchctl", &["bootout", &launchd_service_target()?]);
        std::fs::write(
            &plist,
            render_launchd_plist(&exe, &env, &data_dir.join("daemon.log")),
        )?;
        run(
            "launchctl",
            &["bootstrap", &launchd_domain()?, &plist.to_string_lossy()],
        )?;
        println!(
            "Installed and started {LAUNCHD_LABEL} ({}).",
            plist.display()
        );
    } else if cfg!(target_os = "linux") {
        let unit = systemd_unit_path()?;
        std::fs::create_dir_all(unit.parent().expect("systemd user dir"))?;
        std::fs::write(&unit, render_systemd_unit(&exe, &env))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
        println!("Installed and started {SYSTEMD_UNIT} ({}).", unit.display());
        println!(
            "For start-at-boot without an active login session (VPS): loginctl enable-linger $USER"
        );
    } else if cfg!(target_os = "windows") {
        win::install(&exe, &env)?;
        println!("Installed and started the {SERVICE_NAME} service.");
    } else {
        bail!("komet daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    println!(
        "Without a saved account the engine stays local-only; sign-in and restart are optional for sync."
    );
    println!(
        "Logs: {}",
        if cfg!(target_os = "macos") {
            format!("{}", data_dir.join("daemon.log").display())
        } else {
            format!("journalctl --user -u {SYSTEMD_UNIT}")
        }
    );
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let _ = run_quiet("launchctl", &["bootout", &launchd_service_target()?]);
        let plist = launchd_plist_path()?;
        match std::fs::remove_file(&plist) {
            Ok(()) => println!("Removed {}.", plist.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("Not installed.")
            }
            Err(err) => return Err(err.into()),
        }
    } else if cfg!(target_os = "linux") {
        let _ = run_quiet("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
        let unit = systemd_unit_path()?;
        match std::fs::remove_file(&unit) {
            Ok(()) => {
                run("systemctl", &["--user", "daemon-reload"])?;
                println!("Removed {}.", unit.display());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("Not installed.")
            }
            Err(err) => return Err(err.into()),
        }
    } else if cfg!(target_os = "windows") {
        if win::uninstall()? {
            println!("Removed the {SERVICE_NAME} service.");
        } else {
            println!("Not installed.");
        }
    } else {
        bail!("komet daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path()?;
        if !plist.exists() {
            bail!("not installed — run `komet daemon install` first");
        }
        // `stop` boots the job out of the domain, so start = bootstrap; already
        // loaded is fine, then kickstart guarantees a running process either way.
        let _ = run_quiet(
            "launchctl",
            &["bootstrap", &launchd_domain()?, &plist.to_string_lossy()],
        );
        run("launchctl", &["kickstart", &launchd_service_target()?])?;
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "start", SYSTEMD_UNIT])?;
    } else if cfg!(target_os = "windows") {
        if !win::start()? {
            bail!("not installed — run `komet daemon install` first");
        }
    } else {
        bail!("komet daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    println!("Started.");
    Ok(())
}

pub fn stop() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        // bootout (not `kill`): with KeepAlive the job would otherwise respawn.
        run("launchctl", &["bootout", &launchd_service_target()?])?;
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "stop", SYSTEMD_UNIT])?;
    } else if cfg!(target_os = "windows") {
        win::stop()?;
    } else {
        bail!("komet daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    println!("Stopped.");
    Ok(())
}

pub fn restart() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        if run_quiet(
            "launchctl",
            &["kickstart", "-k", &launchd_service_target()?],
        )
        .is_err()
        {
            // Not loaded (e.g. after `stop`) — fall through to a plain start.
            return start();
        }
        println!("Restarted.");
        Ok(())
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "restart", SYSTEMD_UNIT])?;
        println!("Restarted.");
        Ok(())
    } else if cfg!(target_os = "windows") {
        win::restart()?;
        println!("Restarted.");
        Ok(())
    } else {
        bail!("komet daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
}

pub fn status() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let output = Command::new("launchctl")
            .args(["print", &launchd_service_target()?])
            .output()
            .context("running launchctl")?;
        if !output.status.success() {
            println!(
                "{LAUNCHD_LABEL}: not loaded{}",
                if launchd_plist_path()?.exists() {
                    " (installed — `komet daemon start`)"
                } else {
                    " (not installed — `komet daemon install`)"
                }
            );
            return Ok(());
        }
        // `launchctl print` is pages long; surface just the liveness lines.
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{LAUNCHD_LABEL}: loaded");
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("state = ")
                || trimmed.starts_with("pid = ")
                || trimmed.starts_with("last exit code = ")
            {
                println!("  {trimmed}");
            }
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        // Passthrough; `status` exits nonzero for inactive units, which is not an
        // error for us to report — the output already says it.
        let _ = Command::new("systemctl")
            .args(["--user", "--no-pager", "status", SYSTEMD_UNIT])
            .status()
            .context("running systemctl")?;
        Ok(())
    } else if cfg!(target_os = "windows") {
        win::status()?;
        Ok(())
    } else {
        bail!("komet daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
}

// ---------------------------------------------------------------------------
// Unit rendering (pure — unit-tested below)
// ---------------------------------------------------------------------------

fn captured_env() -> Vec<(String, String)> {
    CAPTURED_ENV
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key.to_string(), v)))
        .collect()
}

fn render_systemd_unit(exe: &Path, env: &[(String, String)]) -> String {
    let mut unit = String::from(
        "[Unit]\nDescription=Komet headless engine\nAfter=network-online.target\nStartLimitIntervalSec=60\nStartLimitBurst=5\n\n[Service]\n",
    );
    for (key, value) in env {
        // systemd unquotes the value; escape the characters it treats specially.
        let value = value.replace('\\', "\\\\").replace('"', "\\\"");
        unit.push_str(&format!("Environment=\"{key}={value}\"\n"));
    }
    unit.push_str(&format!(
        "ExecStart={} headless\nRestart=on-failure\nRestartSec=5\nEnvironmentFile=-%h/.komet/env\n\n[Install]\nWantedBy=default.target\n",
        systemd_exec_path(exe)
    ));
    unit
}

/// The ExecStart binary path. An exe under `~/.komet/app/` came from the
/// curl|sh installer, whose upgrades relink `app/current` — point the unit at
/// the symlink (as the installer's own unit does) so it never pins one version.
/// (`current_exe` resolves symlinks, so the versioned dir is what we see here.)
fn systemd_exec_path(exe: &Path) -> String {
    exec_path_for(exe, std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn exec_path_for(exe: &Path, home: Option<&Path>) -> String {
    let installed = home
        .map(|home| home.join(".komet/app"))
        .is_some_and(|app_root| exe.starts_with(app_root));
    if installed {
        "%h/.komet/app/current/komet".to_string()
    } else {
        format!("{}", exe.display())
    }
}

fn render_launchd_plist(exe: &Path, env: &[(String, String)], log: &Path) -> String {
    let mut env_dict = String::new();
    for (key, value) in env {
        env_dict.push_str(&format!(
            "      <key>{}</key><string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{exe}</string>
      <string>headless</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env_dict}    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
      <key>SuccessfulExit</key><false/>
    </dict>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
  </dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = xml_escape(&exe.to_string_lossy()),
        env_dict = env_dict,
        log = xml_escape(&log.to_string_lossy()),
    )
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Paths + process helpers
// ---------------------------------------------------------------------------

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

fn launchd_plist_path() -> anyhow::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn systemd_unit_path() -> anyhow::Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config"));
    Ok(config.join("systemd/user").join(SYSTEMD_UNIT))
}

fn launchd_domain() -> anyhow::Result<String> {
    let output = Command::new("id").arg("-u").output().context("id -u")?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("could not determine the current uid");
    }
    Ok(format!("gui/{uid}"))
}

fn launchd_service_target() -> anyhow::Result<String> {
    Ok(format!("{}/{LAUNCHD_LABEL}", launchd_domain()?))
}

/// Run a command echoing it first; error (with stderr) on nonzero exit.
fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    println!("$ {program} {}", args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Run without echoing; used where failure is an expected branch.
fn run_quiet(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!("{program} failed ({})", output.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_shape() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/bin/komet"),
            &[
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("KOMET_EDGE_URL".into(), "https://edge.example".into()),
                ("RUST_LOG".into(), "info,komet=\"debug\"".into()),
            ],
        );
        assert!(unit.contains("ExecStart=/usr/local/bin/komet headless\n"));
        assert!(unit.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(unit.contains("Environment=\"KOMET_EDGE_URL=https://edge.example\"\n"));
        // Inner quotes escaped so systemd re-parses the value verbatim.
        assert!(unit.contains("Environment=\"RUST_LOG=info,komet=\\\"debug\\\"\"\n"));
        assert!(unit.contains("StartLimitIntervalSec=60\n"));
        assert!(unit.contains("StartLimitBurst=5\n"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(!unit.contains("session.json"));
        assert!(!unit.contains("ConditionPathExists"));
        assert!(unit.contains("EnvironmentFile=-%h/.komet/env"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn curl_installer_always_starts_the_local_capable_service() {
        let installer = include_str!("../../../edge/src/install.sh");
        assert!(!installer.contains("session.json"));
        assert!(installer.contains("StartLimitIntervalSec=60\n"));
        assert!(installer.contains("StartLimitBurst=5\n"));
        assert!(installer.contains("systemctl --user enable komet"));
        assert!(installer.contains("systemctl --user restart komet"));
    }

    #[test]
    fn installed_exe_uses_the_current_symlink() {
        // Installer-managed binary (current_exe resolves the `current` symlink to
        // the versioned dir): the unit must point back at the symlink.
        assert_eq!(
            exec_path_for(
                Path::new("/home/u/.komet/app/0.3.0/komet"),
                Some(Path::new("/home/u")),
            ),
            "%h/.komet/app/current/komet"
        );
        // Source build: literal path.
        assert_eq!(
            exec_path_for(
                Path::new("/src/target/debug/komet"),
                Some(Path::new("/home/u"))
            ),
            "/src/target/debug/komet"
        );
    }

    #[test]
    fn launchd_plist_shape() {
        let plist = render_launchd_plist(
            Path::new("/Users/x/komet & co/komet"),
            &[("KOMET_EDGE_URL".into(), "https://e?a=1&b=2".into())],
            Path::new("/Users/x/.komet/daemon.log"),
        );
        assert!(plist.contains("<key>Label</key><string>sh.komet.app</string>"));
        // XML-escaped exe path and env value.
        assert!(plist.contains("<string>/Users/x/komet &amp; co/komet</string>"));
        assert!(plist.contains("<string>https://e?a=1&amp;b=2</string>"));
        assert!(plist.contains("<string>headless</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        assert!(
            plist.contains("<key>StandardOutPath</key><string>/Users/x/.komet/daemon.log</string>")
        );
    }
}
