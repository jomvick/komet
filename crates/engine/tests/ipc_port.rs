//! The engine port has exactly one owner, and only that owner's token is
//! published for it.

use std::sync::Arc;

use komet_engine::{EngineCore, default_registry, serve_ipc};
use komet_proto::HarnessId;

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn a_second_engine_cannot_take_the_port_or_replace_its_token() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let core_a = EngineCore::assemble(
        dir_a.path(),
        Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let core_b = EngineCore::assemble(
        dir_b.path(),
        Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let port = free_port().await;

    let server_a = serve_ipc(port, core_a.rpc_service()).await.unwrap();
    let token_a = komet_rpc::ipc_token::read_default(port).unwrap();

    // Different data directories, so the instance lock does not stop engine B:
    // the port bind itself must.
    assert!(
        serve_ipc(port, core_b.rpc_service()).await.is_err(),
        "a second engine must not bind a port another engine owns"
    );
    assert_eq!(komet_rpc::ipc_token::read_default(port).unwrap(), token_a);

    let client = komet_rpc::connect_ws(&format!("ws://127.0.0.1:{port}"), &token_a)
        .await
        .expect("clients using the published token still reach the owner");
    drop(client);
    server_a.abort();
}
