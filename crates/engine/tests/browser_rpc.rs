//! Browser host RPC against real Chrome (issue #45). Ignored: needs Chrome
//! (`CHROME_PATH` or a well-known location). Exercises the exact path the
//! Browser surface uses: navigate → frame → input → history → close, and
//! the run-time rewrite of the browser-use preset to `--browserUrl`.

use std::sync::Arc;

use komet_engine::mcp::browser_use;
use komet_engine::{EngineCore, EngineRpc};
use komet_proto::HarnessId;
use komet_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;

fn test_engine() -> (EngineCore, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(komet_engine::default_registry()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    (core, dir)
}

async fn call(
    rpc: &EngineRpc,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    match rpc.handle(method, params).await? {
        RpcReply::Value(v) => Ok(v),
        RpcReply::Stream(_) => panic!("{method}: unexpected stream reply"),
    }
}

#[tokio::test]
async fn frame_before_launch_reports_not_running() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let v = call(&rpc, methods::BROWSER_FRAME, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(v["running"], false);
    let err = call(&rpc, methods::BROWSER_INPUT, serde_json::json!({ "kind": "nope" }))
        .await
        .unwrap_err();
    assert!(matches!(err, RpcError::BadParams(_)));
}

#[tokio::test]
#[ignore]
async fn navigate_frame_input_history_close_with_real_chrome() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();

    let page = "data:text/html,<title>Komet e2e</title><input id=i autofocus><p>hello</p>";
    let v = call(&rpc, methods::BROWSER_NAVIGATE, serde_json::json!({ "url": page }))
        .await
        .expect("navigate");
    assert_eq!(v["title"], "Komet e2e");
    assert!(v["imageBase64"].as_str().is_some_and(|b| b.len() > 1000));
    assert_eq!(v["canGoBack"], true);

    let status = call(&rpc, methods::BROWSER_USE_STATUS, serde_json::json!({}))
        .await
        .unwrap();
    let browser_url = status["browser"]["browserUrl"].as_str().unwrap().to_string();
    assert!(browser_url.starts_with("http://127.0.0.1:"));

    // Preset rewrite for runs: engine-owned Chrome via --browserUrl.
    let mut cfg = browser_use::server_config(false);
    browser_use::attach_to_browser(&mut cfg, &browser_url);
    assert!(cfg.args.iter().any(|a| a == &browser_url));

    for params in [
        serde_json::json!({ "kind": "click", "x": 20.0, "y": 20.0 }),
        serde_json::json!({ "kind": "text", "text": "abc" }),
        serde_json::json!({ "kind": "key", "key": "enter" }),
        serde_json::json!({ "kind": "scroll", "x": 10.0, "y": 10.0, "dx": 0.0, "dy": 100.0 }),
    ] {
        call(&rpc, methods::BROWSER_INPUT, params).await.unwrap();
    }
    let v = call(&rpc, methods::BROWSER_HISTORY, serde_json::json!({ "delta": -1 }))
        .await
        .unwrap();
    assert_eq!(v["url"], "about:blank");
    let v = call(&rpc, methods::BROWSER_HISTORY, serde_json::json!({ "delta": 1 }))
        .await
        .unwrap();
    assert_eq!(v["title"], "Komet e2e");
    let v = call(&rpc, methods::BROWSER_RELOAD, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(v["title"], "Komet e2e");

    call(&rpc, methods::BROWSER_CLOSE, serde_json::json!({}))
        .await
        .unwrap();
    let v = call(&rpc, methods::BROWSER_FRAME, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(v["running"], false);
}
