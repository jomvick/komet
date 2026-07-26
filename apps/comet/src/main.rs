//! komet — headed by default; `komet headless` runs the engine alone. Auth is
//! decoupled from the daemon: `komet login` persists the session and exits, so a
//! service-managed `komet headless` only ever loads saved credentials.

mod auth_cli;
mod daemon;

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
    /// Terminal viewport over the same engine — attaches to a running app or
    /// daemon, or starts one, and detaches (leaving work running) when it exits.
    Tui(komet_tui::cli::TuiArgs),
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The TUI owns its own tracing (to a file — a line on stdout would land
    // inside the alternate screen and corrupt it), so skip the global stdout
    // subscriber entirely for it. Everything else logs to stdout: long-running
    // modes at info, one-shot CLI commands at warn (RUST_LOG overrides either).
    if !matches!(cli.command, Some(Command::Tui(_))) {
        let default_filter = match &cli.command {
            None | Some(Command::Headless) => "info",
            Some(_) => "warn",
        };
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| default_filter.into()),
            )
            .init();
    }

    match cli.command {
        Some(Command::Tui(args)) => komet_tui::cli::run(args),
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
