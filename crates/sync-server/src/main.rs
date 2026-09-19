//! Standalone sync server binary for the container image. Same behaviour as
//! `komet sync-server`, without building the desktop app.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let token = komet_sync_server::require_token(std::env::var("KOMET_SYNC_TOKEN").ok())?;
    let data_dir = std::env::var_os("KOMET_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));
    let port = match std::env::var("KOMET_SYNC_PORT") {
        Ok(value) => value
            .parse()
            .map_err(|_| anyhow::anyhow!("KOMET_SYNC_PORT is not a valid port: {value}"))?,
        Err(_) => 8787,
    };
    tokio::runtime::Runtime::new()?.block_on(komet_sync_server::serve(data_dir, token, port))
}
