//! Windows service-manager integration: `komet daemon …` maps to SCM
//! install/uninstall/start/stop/restart/status, and the hidden `--service`
//! flag is the SCM entry point that runs the headless engine.
//!
//! The service runs under the installing user's account (not LocalSystem) so
//! `%USERPROFILE%`-scoped files (`.claude/`, saved sessions) resolve exactly
//! as an interactive launch. The KOMET_* environment captured at install time
//! is persisted under the service's registry key and re-applied at boot —
//! SCM starts services with a minimal environment.
//!
//! Stop flows through the same channel `komet daemon stop` and the UI use:
//! the `SERVICE_CONTROL_STOP` handler only flips an atomic flag (the
//! handler box is freed by the crate once Stop is delivered), the service
//! loop observes it and asks the engine to stop via the IPC STOP_ENGINE
//! method, then waits for the engine thread before reporting `Stopped`.

#[cfg(target_os = "windows")]
use std::ffi::OsString;
#[cfg(target_os = "windows")]
use std::path::Path;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::time::Duration;

#[cfg(target_os = "windows")]
use anyhow::{Context, bail};
#[cfg(target_os = "windows")]
use winreg::enums::HKEY_LOCAL_MACHINE;
#[cfg(target_os = "windows")]
use winreg::RegKey;
#[cfg(target_os = "windows")]
use windows_service::service::{
    Service, ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
#[cfg(target_os = "windows")]
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
#[cfg(target_os = "windows")]
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

#[cfg(target_os = "windows")]
use super::{CAPTURED_ENV, SERVICE_NAME};

#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
const DISPLAY_NAME: &str = "Komet headless engine";
#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
const ENV_REGISTRY_PATH: &str =
    r"SYSTEM\CurrentControlSet\Services\Komet\Parameters\Environment";
/// How long to wait for a StopPending → Stopped transition before giving up.
#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
const STOP_WAIT: Duration = Duration::from_secs(30);

#[cfg(target_os = "windows")]
windows_service::define_windows_service!(ffi_service_main, service_main);

#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access).context("opening the service control manager")
}

#[cfg(target_os = "windows")]
pub fn install(exe: &Path, env: &[(String, String)]) -> anyhow::Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;

    // Reinstall-friendly (upgrades, changed env): drop any previous service.
    if let Ok(service) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        let _ = stop_and_wait(&service);
        let _ = service.delete();
    }

    let (account_name, account_password, local_system) = match std::env::var("KOMET_SERVICE_PASSWORD")
    {
        Ok(password) if !password.trim().is_empty() => {
            (Some(service_account_name()?.into()), Some(password.into()), false)
        }
        _ => (None, None, true),
    };

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![OsString::from("--service")],
        dependencies: Vec::new(),
        account_name,
        account_password,
    };
    let service = manager
        .create_service(&info, ServiceAccess::ALL_ACCESS)
        .context("creating the service")?;

    persist_env(env)?;
    service
        .start(&[] as &[OsString])
        .context("starting the service")?;

    if local_system {
        println!(
            "WARNING: installed as LocalSystem — the engine cannot reach the user's %USERPROFILE%\n\
             (Claude config, saved sessions). Re-run install with KOMET_SERVICE_PASSWORD=<account\n\
             password> to run as the current user."
        );
    }
    Ok(())
}

/// The SCM account string for the installing user: `DOMAIN\user` for domain
/// accounts, `.\user` for local ones (which is what the installer/MSI targets).
#[cfg(target_os = "windows")]
fn service_account_name() -> anyhow::Result<String> {
    let user = std::env::var("USERNAME").context("USERNAME not set")?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    if !domain.is_empty() && !domain.eq_ignore_ascii_case(&user) {
        Ok(format!("{domain}\\{user}"))
    } else {
        Ok(format!(".\\{user}"))
    }
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> anyhow::Result<bool> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Ok(service) =
        manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
    else {
        return Ok(false);
    };
    let _ = stop_and_wait(&service);
    service.delete().context("deleting the service")?;
    let _ = RegKey::predef(HKEY_LOCAL_MACHINE).delete_subkey_all(ENV_REGISTRY_PATH);
    Ok(true)
}

#[cfg(target_os = "windows")]
pub fn start() -> anyhow::Result<bool> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::START) else {
        return Ok(false);
    };
    service.start(&[] as &[OsString]).context("starting the service")?;
    Ok(true)
}

#[cfg(target_os = "windows")]
pub fn stop() -> anyhow::Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
        .with_context(|| format!("opening {SERVICE_NAME} (not installed?)"))?;
    stop_and_wait(&service)?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn restart() -> anyhow::Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .with_context(|| format!("opening {SERVICE_NAME} (not installed?)"))?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        stop_and_wait(&service)?;
    }
    service.start(&[] as &[OsString]).context("restarting the service")?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn status() -> anyhow::Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) else {
        println!("{SERVICE_NAME}: not installed (`komet daemon install`)");
        return Ok(());
    };
    let status = service.query_status().context("querying service status")?;
    println!("{SERVICE_NAME}: {:?}", status.current_state);
    if let Some(pid) = status.process_id {
        println!("  pid = {pid}");
    }
    Ok(())
}

/// Ask SCM to stop the service and wait for the transition.
#[cfg(target_os = "windows")]
fn stop_and_wait(service: &Service) -> anyhow::Result<()> {
    let status = service.query_status().context("querying service status")?;
    if status.current_state == ServiceState::Stopped {
        return Ok(());
    }
    service.stop().context("sending the stop request")?;
    let deadline = std::time::Instant::now() + STOP_WAIT;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        match service.query_status() {
            Ok(status) if status.current_state == ServiceState::Stopped => return Ok(()),
            Ok(_) if std::time::Instant::now() < deadline => continue,
            Ok(_) => bail!("timed out waiting for {SERVICE_NAME} to stop"),
            Err(err) => bail!("querying status while stopping: {err}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry env persistence
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn env_key() -> anyhow::Result<RegKey> {
    Ok(RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey(ENV_REGISTRY_PATH)
        .context("opening the service registry key")?
        .0)
}

/// Persist the captured environment under the service key; drop keys that are
/// no longer captured (an install with fewer overrides must not leave stale
/// values behind).
#[cfg(target_os = "windows")]
fn persist_env(env: &[(String, String)]) -> anyhow::Result<()> {
    let key = env_key()?;
    for (name, value) in env {
        key.set_value(name, value)
            .with_context(|| format!("writing registry value {name}"))?;
    }
    let names: Vec<String> = key
        .enum_values()
        .filter_map(|entry| entry.ok().map(|(name, _)| name))
        .collect();
    for name in names {
        if !env.iter().any(|(k, _)| k == &name) {
            let _ = key.delete_value(&name);
        }
    }
    Ok(())
}

/// Re-apply the persisted environment. Called from the service entry point —
/// SCM starts the process with a minimal environment (no `KOMET_*`, and only
/// the system PATH), so the engine must be handed back what install captured.
#[cfg(target_os = "windows")]
pub fn load_persisted_env() {
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(ENV_REGISTRY_PATH) else {
        return;
    };
    for entry in key.enum_values() {
        let Ok((name, _)) = entry else {
            continue;
        };
        if !CAPTURED_ENV.contains(&name.as_str()) {
            continue;
        }
        if let Ok(value) = key.get_value::<String, _>(&name) {
            // SAFETY: runs once at service start before any other thread reads
            // these vars. `set_var` is still `unsafe` on this toolchain (the
            // "safe set_var" change is edition-gated); allow the shim either
            // way so the build survives both.
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var(name, value);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The service entry point
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn service_main(_arguments: Vec<OsString>) {
    load_persisted_env();

    let stop = Arc::new(AtomicBool::new(false));
    let handler_stop = stop.clone();
    let status_handle = match service_control_handler::register(SERVICE_NAME, move |control| {
        match control {
            // The handler box is freed by the crate once Stop/Shutdown is
            // delivered — so it must not own any shutdown work. Flag only;
            // the loop below observes the flag and stops the engine.
            ServiceControl::Stop | ServiceControl::Shutdown => {
                handler_stop.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("komet: failed to register the control handler: {err}");
            return;
        }
    };

    let running = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    if let Err(err) = status_handle.set_service_status(running) {
        eprintln!("komet: failed to report Running: {err}");
        return;
    }

    // The engine owns its tokio runtime; run it on a dedicated thread so the
    // service main can drive the stop handshake without blocking it.
    let engine_thread = std::thread::spawn(|| -> anyhow::Result<()> {
        let runtime = tokio::runtime::Runtime::new()
            .with_context(|| "failed to start the engine runtime")?;
        runtime.block_on(async {
            let engine = komet_engine::Engine::new(crate::engine_config_from_env());
            engine.run().await
        })
    });

    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
    }

    // Graceful stop, same path the UI / `komet daemon stop` rely on: ask the
    // engine over its IPC port and let it finish in-flight work.
    if let Err(err) = request_stop() {
        eprintln!("komet: stop request failed ({err}); waiting for the engine to exit");
    }
    let _ = engine_thread.join();

    let stopped = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    let _ = status_handle.set_service_status(stopped);
}

/// Send the engine the same IPC STOP_ENGINE call the `komet daemon stop` /
/// UI flow uses; returns without error even when no engine is reachable.
#[cfg(target_os = "windows")]
fn request_stop() -> anyhow::Result<()> {
    let ipc_port = std::env::var("KOMET_IPC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(27654);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let client = komet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}")).await?;
        client
            .call(komet_rpc::methods::STOP_ENGINE, serde_json::json!({}))
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Called from `main` for the hidden `--service` flag: hand the SCM entry
/// point to the dispatcher, which blocks until the service stops.
#[cfg(target_os = "windows")]
pub fn run_as_windows_service() -> anyhow::Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting the service dispatcher")
}

// ---------------------------------------------------------------------------
// Non-Windows stubs. The `cfg!(target_os = "windows")` branches in
// `daemon.rs` are type-checked on every platform, so these must exist. They
// are unreachable at runtime — daemon dispatch bails before the `cfg!` test
// would select them — so a panic is correct and keeps them trivial.
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
pub fn install(_exe: &std::path::Path, _env: &[(String, String)]) -> anyhow::Result<()> {
    unreachable!("komet daemon is only supported on macOS (launchd) and Linux (systemd)")
}

#[cfg(not(target_os = "windows"))]
pub fn uninstall() -> anyhow::Result<bool> {
    unreachable!("komet daemon is only supported on macOS (launchd) and Linux (systemd)")
}

#[cfg(not(target_os = "windows"))]
pub fn start() -> anyhow::Result<bool> {
    unreachable!("komet daemon is only supported on macOS (launchd) and Linux (systemd)")
}

#[cfg(not(target_os = "windows"))]
pub fn stop() -> anyhow::Result<()> {
    unreachable!("komet daemon is only supported on macOS (launchd) and Linux (systemd)")
}

#[cfg(not(target_os = "windows"))]
pub fn restart() -> anyhow::Result<()> {
    unreachable!("komet daemon is only supported on macOS (launchd) and Linux (systemd)")
}

#[cfg(not(target_os = "windows"))]
pub fn status() -> anyhow::Result<()> {
    unreachable!("komet daemon is only supported on macOS (launchd) and Linux (systemd)")
}