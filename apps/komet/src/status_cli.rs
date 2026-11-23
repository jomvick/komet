//! `komet status` — report the active sync configuration and engine listener.

use komet_engine::{Engine, EngineConfig, InstanceLock};

/// Report local runtime state without contacting the sync server.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let sync_configured = !config.edge_url.trim().is_empty()
        && config
            .sync_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty());
    let next_scope = Engine::initial_workspace_scope();
    let scope = live_engine_scope(config.ipc_port)
        .await
        .unwrap_or(next_scope);

    println!("Data dir: {}", config.data_dir.display());
    println!(
        "Sync:     {}",
        if sync_configured {
            config.edge_url.as_str()
        } else {
            "disabled (set KOMET_EDGE_URL and KOMET_SYNC_TOKEN)"
        }
    );
    println!("Scope:    {scope:?}");
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() {
            "listening on"
        } else {
            "not listening on"
        },
        config.ipc_port
    );
    Ok(())
}

async fn live_engine_scope(ipc_port: u16) -> Option<komet_proto::WorkspaceScope> {
    let token = komet_rpc::ipc_token::read_default(ipc_port).ok()?;
    let client = komet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"), &token)
        .await
        .ok()?;
    let value = client
        .call(komet_rpc::methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .ok()?;
    serde_json::from_value::<komet_engine::EngineInfo>(value)
        .ok()
        .map(|info| info.workspace_scope)
}
