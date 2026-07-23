//! komet — headed by default; `komet headless` runs the engine alone.

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
}

/// Production edge (Cloudflare Worker + Durable Objects on the komet.sh zone).
/// `KOMET_EDGE_URL` overrides (local dev / self-hosting).
const DEFAULT_EDGE_URL: &str = "https://edge.komet.komet.sh";

/// Production WorkOS AuthKit client id — public knowledge (it appears in every
/// authorize URL), so baking it in is safe. Overridden by `KOMET_WORKOS_CLIENT_ID`;
/// set it to the empty string — or set a dev bearer via `KOMET_EDGE_TOKEN` — to
/// force dev-mode auth instead.
const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KWD0E9SYPV2WE4QN9TP9DP0W";

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                // Dev-mode bearer (no WorkOS): an explicit token enables sync.
                let edge_token = std::env::var("KOMET_EDGE_TOKEN").ok();
                let engine = komet_engine::Engine::new(komet_engine::EngineConfig {
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
                });
                engine.run().await
            })
        }
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
