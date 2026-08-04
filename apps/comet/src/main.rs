//! komet — headed by default; `komet headless` runs the engine alone. Auth is
//! decoupled from the daemon: `komet login` persists the session and exits, so a
//! service-managed `komet headless` only ever loads saved credentials.

mod auth_cli;
mod daemon;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "komet", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (VPS / remote device mode).
    Headless,
    /// Sign in (paste-code flow), persist the session, and exit.
    Login,
    /// Remove the saved session.
    Logout,
    /// Show auth + engine status (exits nonzero when a sign-in is needed).
    Status,
    /// Manage `komet headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures KOMET_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

/// Production edge (Cloudflare Worker + Durable Objects on the komet.sh zone).
/// `KOMET_EDGE_URL` overrides (local dev / self-hosting).
const DEFAULT_EDGE_URL: &str = "https://edge.komet.komet.sh";

/// Production WorkOS AuthKit client id — public knowledge (it appears in every
/// authorize URL), so baking it in is safe. Overridden by `KOMET_WORKOS_CLIENT_ID`;
/// set it to the empty string — or set a dev bearer via `KOMET_EDGE_TOKEN` — to
/// force dev-mode auth instead.
const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KWD0EAKZKD50YCQJNYSRE4BY";

fn edge_url_from_env() -> String {
    std::env::var("KOMET_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EDGE_URL.into())
}

/// WorkOS client id resolution: explicit env wins (empty string = dev mode);
/// otherwise a `KOMET_EDGE_TOKEN` dev bearer keeps dev mode (smoke tests,
/// local wrangler); otherwise the baked production client id — so a bare
/// `komet headless` signs in against production with zero configuration.
fn workos_client_id_from_env(edge_token: &Option<String>) -> Option<String> {
    match std::env::var("KOMET_WORKOS_CLIENT_ID") {
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(v),
        Err(_) if edge_token.is_some() => None,
        Err(_) => Some(DEFAULT_WORKOS_CLIENT_ID.into()),
    }
}

/// mimalloc: system malloc (macOS libmalloc especially) never returns the
/// streaming churn's high-water pages, so transient allocation became
/// permanent RSS (docs/memory-plan.md §1).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Long-running modes log at info, one-shot CLI commands at warn (RUST_LOG
    // overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let long_running = matches!(&cli.command, None | Some(Command::Headless));
    let default_filter = if long_running {
        "info,loro_internal=warn,loro=warn"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Long-running modes mirror stdout logging to {data_dir}/logs — a headed
    // app launched from Finder has no visible stdout, which left every sync
    // wedge report ("stale until restart") with zero diagnostics even though
    // the engine logs the exact failure line. One file per launch, previous
    // launch kept as `.old`.
    let log_file = if long_running {
        let mode = if cli.command.is_some() {
            "headless"
        } else {
            "headed"
        };
        open_log_file(mode)
    } else {
        None
    };
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer());
        match log_file {
            Some(file) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Arc::new(file)),
                )
                .init(),
            None => registry.init(),
        }
    }

    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = komet_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Login) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::login(engine_config_from_env()))
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(&edge_url_from_env(), check))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            let edge_token = std::env::var("KOMET_EDGE_TOKEN").ok();
            // Headed: the UI probes KOMET_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            komet_ui::run_app(komet_ui::UiConfig {
                data_dir: std::env::var_os("KOMET_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port: std::env::var("KOMET_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                edge_url: edge_url_from_env(),
                workos_client_id: workos_client_id_from_env(&edge_token),
                edge_token,
                org_id: std::env::var("KOMET_ORG_ID").ok(),
                default_harness: komet_ui::HarnessId::ClaudeCode,
            });
            Ok(())
        }
    }
}

/// The env-resolved engine configuration shared by `headless`, `login`,
/// `logout`, and `status` — one resolution so the CLI auth commands always
/// operate on the exact session the daemon will load.
fn engine_config_from_env() -> komet_engine::EngineConfig {
    // Dev-mode bearer (no WorkOS): an explicit token enables sync.
    let edge_token = std::env::var("KOMET_EDGE_TOKEN").ok();
    komet_engine::EngineConfig {
        data_dir: std::env::var_os("KOMET_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        edge_url: edge_url_from_env(),
        ipc_port: std::env::var("KOMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        // WorkOS mode: the signed-in session's org wins; KOMET_ORG_ID (dev
        // default "dev-org") scopes the workspace room otherwise.
        org_id: std::env::var("KOMET_ORG_ID").ok(),
        // Real auth against production by default; see
        // `workos_client_id_from_env` for the dev-mode escape hatches.
        workos_client_id: workos_client_id_from_env(&edge_token),
        edge_token,
    }
}

/// `KOMET_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> komet_engine::HarnessId {
    match std::env::var("KOMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => komet_engine::HarnessId::Mock,
        Ok("codex") => komet_engine::HarnessId::Codex,
        Ok("cursor") => komet_engine::HarnessId::Cursor,
        _ => komet_engine::HarnessId::ClaudeCode,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".komet-native")
}

/// `{data_dir}/logs/komet-{mode}.log`, previous launch preserved as `.old`.
/// Headed and headless are separate files so an embedded-engine app and a
/// daemon on the same machine never interleave writes.
fn open_log_file(mode: &str) -> Option<std::fs::File> {
    let dir = std::env::var_os("KOMET_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir)
        .join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("komet-{mode}.log"));
    let _ = std::fs::rename(&path, dir.join(format!("komet-{mode}.log.old")));
    std::fs::File::create(&path).ok()
}
